use std::cell::Cell;
use std::time::Duration;

use super::*;
use crate::fleet_store::fetch::{load, Policy};
use crate::fleet_store::test_support::{location, sample_files, FakeForge};

fn admins_of(files: std::collections::BTreeMap<String, String>) -> Admins {
    let forge = FakeForge::new(files);
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let loaded = load(&forge, dir.path(), &location(), Policy::FailClosed, now).unwrap();
    from_loaded(&loaded, now)
}

fn with_admins(body: &str) -> std::collections::BTreeMap<String, String> {
    let mut f = sample_files();
    f.insert(ADMINS_PATH.to_string(), body.to_string());
    f
}

#[test]
fn roster_is_fetched_through_the_store_contract() {
    let a = admins_of(with_admins(r#"{"admins":["turian"," rjwalters ","x[bot]",""]}"#));
    assert_eq!(a.logins, vec!["turian", "rjwalters"], "app-spelled entries dropped");
    assert!(a.is_loaded(), "{}", a.state);
}

#[test]
fn missing_file_is_unavailable() {
    let a = admins_of(sample_files());
    assert!(a.logins.is_empty());
    assert!(
        a.state.starts_with("unavailable") && a.state.contains(ADMINS_PATH),
        "{}",
        a.state
    );
}

#[test]
fn malformed_json_is_unavailable() {
    let a = admins_of(with_admins("{not json"));
    assert!(a.logins.is_empty());
    assert!(a.state.contains("malformed"), "{}", a.state);
}

#[test]
fn wrong_type_is_unavailable() {
    for body in [
        r#"{"admins":"turian"}"#,
        r#"["turian"]"#,
        r#"{"admins":{"a":1}}"#,
    ] {
        let a = admins_of(with_admins(body));
        assert!(a.logins.is_empty(), "{body}");
        assert!(!a.is_loaded(), "{body}");
    }
}

#[test]
fn fetch_error_with_no_cache_is_unavailable() {
    let forge = FakeForge::new(sample_files());
    forge.offline.set(true);
    let dir = tempfile::tempdir().unwrap();
    let r = load(&forge, dir.path(), &location(), Policy::AllowStale, chrono::Utc::now());
    assert!(r.is_err(), "no cache, no forge: nothing to read");
}

#[test]
fn stale_cache_beyond_max_age_is_unavailable() {
    let forge = FakeForge::new(with_admins(r#"{"admins":["turian"]}"#));
    let dir = tempfile::tempdir().unwrap();
    let t0 = chrono::Utc::now();
    load(&forge, dir.path(), &location(), Policy::FailClosed, t0).unwrap();
    forge.offline.set(true);
    let soon = t0 + chrono::Duration::hours(1);
    let a = from_loaded(
        &load(&forge, dir.path(), &location(), Policy::AllowStale, soon).unwrap(),
        soon,
    );
    assert_eq!(a.logins, vec!["turian"], "a young stale cache is served");
    let late = t0 + chrono::Duration::hours(25);
    let a = from_loaded(
        &load(&forge, dir.path(), &location(), Policy::AllowStale, late).unwrap(),
        late,
    );
    assert!(a.logins.is_empty() && a.state.contains("24h"), "{}", a.state);
}

#[test]
fn second_resolve_within_ttl_does_not_fetch() {
    let cache = Cache::default();
    let fetches = Cell::new(0);
    let load = || {
        fetches.set(fetches.get() + 1);
        Admins {
            logins: vec!["turian".into()],
            state: "loaded 1".into(),
        }
    };
    let ttl = Duration::from_secs(300);
    cache.get_or_load("a/b@main", ttl, load);
    let again = cache.get_or_load("a/b@main", ttl, load);
    assert_eq!(fetches.get(), 1);
    assert_eq!(again.logins, vec!["turian"]);
    cache.get_or_load("a/b@main", Duration::ZERO, load);
    assert_eq!(fetches.get(), 2, "an expired entry reloads");
}
