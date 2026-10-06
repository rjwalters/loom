//! W8: visibility read from the installation snapshot — fail-private on a
//! failed, stale or absent answer, and the user-credential fallback to the
//! per-repo probe.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::forge_repo_facts::installation::tests::{page, Listing};
use crate::forge_repo_facts::installation::{lookup_repo_with, Answer, Credential, RepoEntry};
use crate::forge_repo_facts::test_support::Env;

fn entry(private: bool) -> Answer {
    Answer::Listed(Some(RepoEntry {
        id: 1,
        full_name: "acme/x".into(),
        private,
    }))
}

#[test]
fn only_a_fresh_listing_saying_public_is_public() {
    assert_eq!(snapshot_visibility(&entry(false)), Some(RepoVisibility::Public));
    assert_eq!(snapshot_visibility(&entry(true)), Some(RepoVisibility::Private));
    assert_eq!(snapshot_visibility(&Answer::Listed(None)), Some(RepoVisibility::Private));
    assert_eq!(snapshot_visibility(&Answer::Unavailable), Some(RepoVisibility::Private));
    assert_eq!(snapshot_visibility(&Answer::PerRepo), None);
    assert_eq!(snapshot_visibility(&Answer::Disabled), None);
}

/// An unavailable snapshot is Private even when the per-repo cache still
/// remembers the repo as Public: the snapshot never falls back to a stale
/// "public".
#[test]
fn an_unavailable_snapshot_ignores_a_remembered_public() {
    let key = "test-owner/w8-remembered-public";
    cache().lock().unwrap().insert(
        key.to_string(),
        VisibilityEntry {
            visibility: RepoVisibility::Public,
            updated_at: Instant::now(),
        },
    );
    assert_eq!(derive_visibility_from(key, Answer::Unavailable), RepoVisibility::Private);
    assert_eq!(derive_visibility_from(key, Answer::Listed(None)), RepoVisibility::Private);
}

fn writer(env: &Env) -> Credential {
    Credential::writer(Some(env.tmp.path().join("cfg-writer")))
}

fn per_repo_calls(fake: &Listing) -> usize {
    fake.calls()
        .iter()
        .filter(|c| c.contains("repos/") && !c.contains("installation/"))
        .count()
}

#[test]
fn a_failed_snapshot_fetch_is_private_with_no_per_repo_read() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "fail");
    let slug = "acme/w8-failed";
    let answer = lookup_repo_with(&fake.gh, &[writer(&env)], slug);
    assert_eq!(derive_visibility_from(slug, answer), RepoVisibility::Private);
    assert_eq!(per_repo_calls(&fake), 0);
}

/// Page 2 failing mid-listing: a repo page 1 listed as public is private.
#[test]
fn a_listing_that_fails_on_a_later_page_is_private() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    let first: Vec<(u64, String, bool)> = (1..=100)
        .map(|i| (i, format!("acme/w8-p{i}"), false))
        .collect();
    let rows: Vec<(u64, &str, bool)> = first.iter().map(|(i, n, p)| (*i, n.as_str(), *p)).collect();
    fake.set("page1", &page(101, &rows));
    fake.set("mode", "failpage2");
    let slug = "acme/w8-p1";
    let answer = lookup_repo_with(&fake.gh, &[writer(&env)], slug);
    assert_eq!(answer, Answer::Unavailable);
    assert_eq!(derive_visibility_from(slug, answer), RepoVisibility::Private);
    assert_eq!(per_repo_calls(&fake), 0);
}

/// The slug's case does not matter, to either answer.
#[test]
fn visibility_ignores_the_slugs_case() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(2, &[(1, "Acme/W8-Open", false), (2, "Acme/W8-Closed", true)]));
    let creds = [writer(&env)];
    let open = lookup_repo_with(&fake.gh, &creds, "acme/w8-open");
    assert_eq!(derive_visibility_from("acme/w8-open", open), RepoVisibility::Public);
    let closed = lookup_repo_with(&fake.gh, &creds, "ACME/W8-CLOSED");
    assert_eq!(derive_visibility_from("ACME/W8-CLOSED", closed), RepoVisibility::Private);
}

#[test]
fn a_repo_absent_from_the_snapshot_is_private() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/listed", false)]));
    let creds = [writer(&env)];
    let listed = lookup_repo_with(&fake.gh, &creds, "acme/listed");
    assert_eq!(derive_visibility_from("acme/listed", listed), RepoVisibility::Public);
    let absent = lookup_repo_with(&fake.gh, &creds, "acme/w8-absent");
    assert_eq!(derive_visibility_from("acme/w8-absent", absent), RepoVisibility::Private);
    assert_eq!(per_repo_calls(&fake), 0);
}

#[test]
fn a_stale_snapshot_is_private() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/w8-stale", false)]));
    let creds = [writer(&env)];
    let slug = "acme/w8-stale";
    let fresh = lookup_repo_with(&fake.gh, &creds, slug);
    assert_eq!(derive_visibility_from(slug, fresh), RepoVisibility::Public);
    crate::forge_repo_facts::advance_test_clock(
        crate::forge_repo_facts::installation::ttl_secs() + 1,
    );
    fake.set("mode", "fail");
    let stale = lookup_repo_with(&fake.gh, &creds, slug);
    assert_eq!(derive_visibility_from(slug, stale), RepoVisibility::Private);
}

#[test]
fn a_304_revalidation_keeps_public() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/w8-304", false)]));
    let creds = [writer(&env)];
    let slug = "acme/w8-304";
    let _ = lookup_repo_with(&fake.gh, &creds, slug);
    crate::forge_repo_facts::advance_test_clock(
        crate::forge_repo_facts::installation::ttl_secs() + 1,
    );
    fake.set("notmodified", "");
    let again = lookup_repo_with(&fake.gh, &creds, slug);
    assert_eq!(derive_visibility_from(slug, again), RepoVisibility::Public);
    assert_eq!(fake.listing_calls(), 2);
}

/// A user credential has no installation listing: visibility takes the
/// per-repo probe, exactly as before W8.
#[test]
#[serial_test::serial(loom_config_env)]
fn a_user_credential_falls_back_to_the_per_repo_probe() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "user");
    fake.set("repo", "HTTP/2.0 200 OK\r\nEtag: W/\"r\"\r\n\r\n{\"private\":false}");
    let slug = "acme/w8-user-credential";
    let answer = lookup_repo_with(&fake.gh, &[writer(&env)], slug);
    assert_eq!(answer, Answer::PerRepo);
    std::env::set_var("LOOM_GH_BIN", &fake.gh);
    let got = derive_visibility_from(slug, answer);
    std::env::remove_var("LOOM_GH_BIN");
    assert_eq!(got, RepoVisibility::Public);
    assert_eq!(per_repo_calls(&fake), 1, "{:?}", fake.calls());
}
