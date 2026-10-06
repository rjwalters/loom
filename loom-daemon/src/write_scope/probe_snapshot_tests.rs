//! W8: the write-scope probe's installation leg from the writer's snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;

use super::*;
use crate::forge_repo_facts::installation::tests::{page, Listing};
use crate::forge_repo_facts::installation::RepoEntry;
use crate::forge_repo_facts::test_support::Env;
use crate::write_scope::probe::{GhProbe, PermissionProbe};

fn listed() -> Answer {
    Answer::Listed(Some(RepoEntry {
        id: 1,
        full_name: "acme/app".into(),
        private: true,
    }))
}

#[test]
fn a_fresh_installation_snapshot_decides_without_leg_one() {
    let ran = Cell::new(0);
    let leg = || {
        ran.set(ran.get() + 1);
        Ok(Some(Permission::Write))
    };
    assert_eq!(from_snapshot(&listed(), leg), Some(Permission::Write));
    assert!(matches!(
        from_snapshot(&Answer::Listed(None), leg),
        Some(Permission::Insufficient(_))
    ));
    assert_eq!(ran.get(), 0);
    assert_eq!(from_snapshot(&Answer::Disabled, leg), None, "pre-W8 legs");
}

#[test]
fn a_user_token_is_decided_by_leg_one() {
    let push = || Ok(Some(Permission::Write));
    assert_eq!(from_snapshot(&Answer::PerRepo, push), Some(Permission::Write));
    let pull = || Ok(Some(Permission::Insufficient("repository role `pull`".into())));
    assert!(matches!(
        from_snapshot(&Answer::PerRepo, pull),
        Some(Permission::Insufficient(_))
    ));
    let failed = || Err("boom".to_string());
    assert!(matches!(from_snapshot(&Answer::PerRepo, failed), Some(Permission::Unknown(_))));
}

/// With no snapshot to be had, only a verified leg-1 WRITE stands: an App
/// token's all-false `permissions` is never turned into a cached refusal or
/// a grant.
#[test]
fn an_unavailable_snapshot_keeps_only_a_leg_one_write() {
    let push = || Ok(Some(Permission::Write));
    assert_eq!(from_snapshot(&Answer::Unavailable, push), Some(Permission::Write));
    let all_false = || Ok(Some(Permission::Insufficient("repository role `none`".into())));
    assert!(matches!(
        from_snapshot(&Answer::Unavailable, all_false),
        Some(Permission::Unknown(_))
    ));
}

/// End to end: the probe reads the writer's own snapshot once, under its
/// own `GH_CONFIG_DIR`, and answers every repo from it.
#[test]
fn the_probe_reads_the_writers_snapshot_once_for_every_repo() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(2, &[(1, "acme/app", true), (2, "acme/site", false)]));
    let cfg = env.tmp.path().join("cfg-writer");
    let probe = GhProbe::new(fake.gh.clone(), Some(cfg));
    assert_eq!(probe.permission("acme/app"), Permission::Write);
    assert_eq!(probe.permission("acme/site"), Permission::Write);
    assert!(matches!(probe.permission("acme/elsewhere"), Permission::Insufficient(_)));
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "one listing, no per-repo reads: {calls:?}");
    assert!(calls[0].contains("cfg-writer"), "{calls:?}");
}

/// A user credential keeps its per-repo read (leg 1).
#[test]
fn a_user_credential_probe_falls_back_to_leg_one() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "user");
    fake.set("repo", r#"{"push":true}"#);
    let probe = GhProbe::new(fake.gh.clone(), Some(env.tmp.path().join("cfg-user")));
    assert_eq!(probe.permission("acme/app"), Permission::Write);
    assert_eq!(probe.permission("acme/other"), Permission::Write);
    assert_eq!(fake.listing_calls(), 1, "the refusal is remembered: {:?}", fake.calls());
}
