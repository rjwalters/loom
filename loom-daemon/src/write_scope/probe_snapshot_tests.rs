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

/// With no snapshot to be had, a verified leg-1 WRITE stands, and an App
/// token's all-false `permissions` is never turned into a cached refusal or
/// a grant.
#[test]
fn an_unavailable_snapshot_keeps_a_leg_one_write_and_not_an_all_false_role() {
    use crate::write_scope::probe::classify_repo_permissions;
    let push = || Ok(Some(Permission::Write));
    assert_eq!(from_snapshot(&Answer::Unavailable, push), Some(Permission::Write));
    // The real classifier's reading of what an App token is served.
    for body in ["{}", r#"{"admin":false,"push":false,"pull":false}"#] {
        let all_false = || Ok(classify_repo_permissions(body));
        assert!(
            matches!(from_snapshot(&Answer::Unavailable, all_false), Some(Permission::Unknown(_))),
            "{body}"
        );
    }
    let unparseable = || Ok(None);
    assert!(matches!(
        from_snapshot(&Answer::Unavailable, unparseable),
        Some(Permission::Unknown(_))
    ));
    let failed = || Err("boom".to_string());
    assert!(matches!(
        from_snapshot(&Answer::Unavailable, failed),
        Some(Permission::Unknown(_))
    ));
}

/// Hardening (e): a NAMED lesser role is a user token's definitive answer.
/// A failed listing does not blur it into a one-minute `Unknown`.
#[test]
fn an_unavailable_snapshot_keeps_a_named_lesser_role_as_insufficient() {
    use crate::write_scope::probe::classify_repo_permissions;
    for (body, role) in [
        (r#"{"admin":false,"push":false,"triage":false,"pull":true}"#, "pull"),
        (r#"{"admin":false,"push":false,"triage":true,"pull":true}"#, "triage"),
    ] {
        let leg = || Ok(classify_repo_permissions(body));
        match from_snapshot(&Answer::Unavailable, leg) {
            Some(Permission::Insufficient(why)) => assert!(why.contains(role), "{why}"),
            other => panic!("{role}: expected Insufficient, got {other:?}"),
        }
    }
}

/// The same end to end: the listing fails, leg 1 says `pull`.
#[test]
fn a_failed_listing_with_a_pull_role_probes_insufficient() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "fail");
    fake.set("repo", r#"{"push":false,"pull":true}"#);
    let probe = GhProbe::new(fake.gh.clone(), Some(env.tmp.path().join("cfg-user")));
    assert!(matches!(probe.permission("acme/app"), Permission::Insufficient(_)));
    fake.set("repo", "{}");
    assert!(matches!(probe.permission("acme/other"), Permission::Unknown(_)));
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
