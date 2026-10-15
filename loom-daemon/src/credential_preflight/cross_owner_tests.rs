//! #11957: per-owner credentials are established after a failed startup mint,
//! retried with bounded backoff, and expire the refusals cached without them.
//!
//! Every test holds the default `serial_test` key: the per-owner registries
//! are process-global, and the `credential_preflight` tests that clear them
//! (`clear_owner_root_registry`) hold that key.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use super::*;
use crate::forge_repo_facts::installation::tests::{page, Listing};
use crate::forge_repo_facts::test_support::Env;
use crate::write_scope::probe::{self, CacheScope, Permission, PermissionProbe};

/// A minter that answers from a script of outcomes (the last one repeats)
/// and counts its calls.
struct ScriptedMinter {
    outcomes: RefCell<Vec<GithubAppOutcome>>,
    calls: Cell<usize>,
}

impl ScriptedMinter {
    fn new(outcomes: Vec<GithubAppOutcome>) -> Self {
        Self {
            outcomes: RefCell::new(outcomes),
            calls: Cell::new(0),
        }
    }
}

impl GithubAppMinter for ScriptedMinter {
    fn mint(&self, _owner_repo: &str) -> GithubAppOutcome {
        self.calls.set(self.calls.get() + 1);
        let mut o = self.outcomes.borrow_mut();
        if o.len() > 1 {
            o.remove(0)
        } else {
            o[0].clone()
        }
    }
}

fn minted() -> GithubAppOutcome {
    GithubAppOutcome::Minted {
        token: "ghs_test_not_a_secret".into(),
        installation_id: "2".into(),
        app_id: "1".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
    }
}

fn timed_out() -> GithubAppOutcome {
    GithubAppOutcome::Error("`bash` timed out after 90s".into())
}

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

/// A probe under the process credential (`key_dir: None`) that counts how
/// often the cache in front of it lets a question through.
struct Counting {
    answer: RefCell<Permission>,
    calls: Cell<usize>,
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

fn unique(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}{}", &id[..12])
}

/// The incident: the startup mint timed out, so no per-owner pass ran and
/// the cross-owner root was gated under the home owner's credential, which
/// refused it as outside its installation and cached that. The first
/// refresh tick that mints again runs the pass: the root gets its owner's
/// credential, the gate admits it, and the cached refusal is expired.
#[test]
#[serial_test::serial(write_scope_cache)]
fn a_failed_startup_mint_then_a_good_refresh_registers_owners_and_clears_refusals() {
    failed_startup_then_refresh_body();
}

/// The gate reads the process-global `LOOM_REPO` / `GH_REPO`, whose test
/// writers hold the default key (nested, as `write_scope_test_support`
/// documents).
#[serial_test::serial]
fn failed_startup_then_refresh_body() {
    let env = Env::new(&[]);
    let _cache = ScratchCache::new(env.tmp.path());
    let fake = Listing::new(env.tmp.path());
    let (home, other) = (unique("home-"), unique("xo-"));
    let (home_repo, other_repo) = (format!("{home}/loom"), format!("{other}/app"));
    let workspace = env.repo("ws", &[("origin", &format!("https://github.com/{home_repo}.git"))]);
    let root = env.repo("b", &[("origin", &format!("https://github.com/{other_repo}.git"))]);
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    let managed = vec![
        (workspace.clone(), home_repo.clone()),
        (root.clone(), other_repo.clone()),
    ];

    // Startup: the primary mint failed, so no pass ran.
    let mut retry = CrossOwnerRetry::after_startup(None);
    assert!(retry.pending());
    assert_eq!(crate::credential_preflight::gh_config_dir_for_root(&root), None);

    // Gated under the process credential, whose installation lists only the
    // home owner's repos: refused, and the refusal is cached.
    fake.set("page1", &page(1, &[(1, &home_repo, false)]));
    let gate = || crate::write_scope::gate_root_with(&root, &fake.gh, "cross-owner test");
    assert!(!gate(), "not in the home owner's installation");
    // The same question asked under the process credential is answered from
    // that cached refusal (the probe behind the cache would say WRITE).
    let process = probe::Cached {
        inner: Counting {
            answer: RefCell::new(Permission::Write),
            calls: Cell::new(0),
        },
        key_dir: None,
    };
    assert!(matches!(process.permission(&other_repo), Permission::Insufficient(_)));
    assert_eq!(process.inner.calls.get(), 0, "the gate's refusal is cached");

    // The refresh tick mints again: the pass is due and establishes the owner.
    assert!(retry.due(), "due on the first good tick after a failed startup");
    let minter = ScriptedMinter::new(vec![minted()]);
    let pass = establish_cross_owner_credentials(&workspace, &home, &managed, &minter);
    retry.record(&pass);
    assert_eq!(pass.established, vec![other.clone()]);
    assert!(pass.complete());
    assert!(!retry.pending(), "no further passes once complete");
    assert!(!retry.due());
    assert_eq!(minter.calls.get(), 1, "one mint, for the cross owner only");

    let owner_dir = github_app_gh_config_dir_for_owner(&workspace, &other);
    assert_eq!(
        crate::credential_preflight::gh_config_dir_for_root(&root),
        Some(owner_dir.clone())
    );
    assert_eq!(
        crate::credential_preflight::gh_config_dir_for_owner_slug(&other_repo),
        Some(owner_dir.clone())
    );
    assert!(owner_dir.join("hosts.yml").exists(), "the owner token is published");

    // The root is now gated under its owner's credential, which lists it.
    fake.set("page1", &page(2, &[(1, &home_repo, false), (2, &other_repo, false)]));
    assert!(gate(), "the cross-owner root may write without a restart");
    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("installation/repositories")
            && c.contains(&*owner_dir.to_string_lossy())),
        "re-probed under the owner's credential: {calls:?}"
    );

    // The refusal cached under the process credential was expired.
    assert_eq!(process.permission(&other_repo), Permission::Write, "re-probed");
    assert_eq!(process.inner.calls.get(), 1);
}

/// A per-owner mint that keeps failing is retried on the next good tick and
/// then with doubling gaps capped at [`MAX_RETRY_TICKS`]; an owner already
/// registered is not re-minted.
#[test]
#[serial_test::serial]
fn a_failing_owner_is_retried_with_bounded_backoff_until_it_registers() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    let (home, ok_owner, bad_owner) = (unique("home-"), unique("ok-"), unique("bad-"));
    let managed: Vec<(PathBuf, String)> = vec![
        (tmp.path().join("ok"), format!("{ok_owner}/one")),
        (tmp.path().join("bad"), format!("{bad_owner}/two")),
    ];
    // ok-owner mints first time; bad-owner fails until the sixth attempt.
    let minter = ScriptedMinter::new(vec![
        minted(),
        timed_out(),
        timed_out(),
        timed_out(),
        timed_out(),
        timed_out(),
        minted(),
    ]);
    let startup = establish_cross_owner_credentials(&workspace, &home, &managed, &minter);
    assert_eq!(startup.established, vec![ok_owner.clone()]);
    assert_eq!(startup.failed, vec![bad_owner.clone()]);
    let mut retry = CrossOwnerRetry::after_startup(Some(&startup));

    // Tick indices (1-based) at which a pass runs: next tick, then gaps of
    // 2, 4, 8, 12 ticks.
    let mut ran_at = Vec::new();
    for tick in 1..=40 {
        if !retry.due() {
            continue;
        }
        ran_at.push(tick);
        let pass = establish_cross_owner_credentials(&workspace, &home, &managed, &minter);
        retry.record(&pass);
        if pass.complete() {
            assert_eq!(pass.established.len(), 2);
            break;
        }
    }
    assert_eq!(ran_at, vec![1, 3, 7, 15, 27]);
    assert!(!retry.pending());
    // 1 (ok) + 1 (bad) at startup, then bad only: ok-owner is already
    // registered and never re-minted.
    assert_eq!(minter.calls.get(), 2 + ran_at.len());
    assert!(crate::credential_preflight::gh_config_dir_for_root(&managed[1].0).is_some());
}

/// A startup pass that completed owes nothing; one with nothing to plan (a
/// single-owner fleet) is complete; an App that turns out unconfigured is
/// not a failure to retry.
#[test]
#[serial_test::serial]
fn a_complete_or_empty_pass_owes_no_retry() {
    let done = CrossOwnerRetry::after_startup(Some(&CrossOwnerPass::default()));
    assert!(!done.pending());
    let tmp = tempfile::tempdir().unwrap();
    let home = unique("home-");
    let single = vec![(tmp.path().join("a"), format!("{home}/a"))];
    let minter = ScriptedMinter::new(vec![GithubAppOutcome::NotConfigured]);
    assert!(establish_cross_owner_credentials(tmp.path(), &home, &single, &minter).complete());
    assert_eq!(minter.calls.get(), 0, "nothing planned, nothing minted");
    let cross = vec![(tmp.path().join("b"), format!("{}/b", unique("xo-")))];
    let pass = establish_cross_owner_credentials(tmp.path(), &home, &cross, &minter);
    assert!(pass.complete() && pass.established.is_empty(), "{pass:?}");
}
