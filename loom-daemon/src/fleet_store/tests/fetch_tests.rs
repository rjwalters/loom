use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::fleet_store::test_support::{location, sample_files, FakeForge};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

#[test]
fn first_sync_fetches_only_contract_files_and_records_the_commit() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    let (snap, changed) = sync(&forge, dir.path(), &location(), t0()).unwrap();
    assert!(changed);
    assert_eq!(snap.manifest.commit, forge.commit());
    assert!(snap.files.contains_key("repos.yml"));
    assert!(snap.files.contains_key("fleet/hosts/build-1/local.json"));
    assert!(!snap.files.contains_key("README.md"), "non-contract files are not fetched");
    assert_eq!(forge.blob_fetches(), 6);
    // The cache round-trips.
    let cached = read_cache(dir.path(), &location()).unwrap().unwrap();
    assert_eq!(cached.files, snap.files);
    assert_eq!(cached.manifest.commit, forge.commit());
}

#[test]
fn unchanged_store_is_one_conditional_request_answered_304() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge.calls.borrow_mut().clear();
    let later = t0() + Duration::hours(1);
    let (snap, changed) = sync(&forge, dir.path(), &location(), later).unwrap();
    assert!(!changed);
    assert_eq!(forge.calls.borrow().len(), 1, "only the ref is revalidated");
    assert_eq!(snap.manifest.confirmed_at, later, "a 304 re-confirms the snapshot");
}

#[test]
fn a_new_commit_refetches_only_changed_blobs() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge
        .files
        .borrow_mut()
        .insert("fleet/state.yml".to_string(), "fleet:\n  state: stopped\n".to_string());
    forge.calls.borrow_mut().clear();
    let (snap, changed) = sync(&forge, dir.path(), &location(), t0()).unwrap();
    assert!(changed);
    assert_eq!(forge.blob_fetches(), 1, "content-addressed blobs are reused");
    assert_eq!(snap.text("fleet/state.yml").unwrap().unwrap(), "fleet:\n  state: stopped\n");
}

#[test]
fn allow_stale_falls_back_to_the_cache_with_a_staleness_warning() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge.offline.set(true);
    let now = t0() + Duration::hours(5);
    let loaded = load(&forge, dir.path(), &location(), Policy::AllowStale, now).unwrap();
    assert!(matches!(loaded.freshness, Freshness::Cached { .. }));
    let warning = loaded.staleness_warning(now).unwrap();
    assert!(warning.contains("CACHED"), "{warning}");
    assert!(warning.contains("5h ago"), "{warning}");
    assert!(warning.contains("network is unreachable"), "{warning}");
}

#[test]
fn fail_closed_refuses_the_cache_when_the_forge_is_unreachable() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge.offline.set(true);
    let err = load(&forge, dir.path(), &location(), Policy::FailClosed, t0()).unwrap_err();
    assert!(format!("{err:#}").contains("could not reach the forge"), "{err:#}");
}

#[test]
fn fail_closed_refuses_on_an_http_error_too() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge.fail_status.set(Some(401));
    let err = load(&forge, dir.path(), &location(), Policy::FailClosed, t0()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("HTTP 401") && msg.contains("Bad credentials"), "{msg}");
}

#[test]
fn fail_closed_is_live_when_the_forge_confirms_the_cache() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    let loaded = load(&forge, dir.path(), &location(), Policy::FailClosed, t0()).unwrap();
    assert_eq!(loaded.freshness, Freshness::Live { changed: false });
    assert!(loaded.staleness_warning(t0()).is_none());
}

#[test]
fn no_cache_and_no_forge_is_an_error_even_when_stale_is_allowed() {
    let forge = FakeForge::new(sample_files());
    forge.offline.set(true);
    let dir = tempfile::tempdir().unwrap();
    let err = load(&forge, dir.path(), &location(), Policy::AllowStale, t0()).unwrap_err();
    assert!(format!("{err:#}").contains("no cached snapshot"), "{err:#}");
}

#[test]
fn offline_never_contacts_the_forge() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    forge.calls.borrow_mut().clear();
    let loaded = load(&forge, dir.path(), &location(), Policy::Offline, t0()).unwrap();
    assert!(forge.calls.borrow().is_empty());
    assert!(matches!(loaded.freshness, Freshness::Cached { .. }));
}

#[test]
fn a_cache_for_another_ref_is_not_used() {
    let forge = FakeForge::new(sample_files());
    let dir = tempfile::tempdir().unwrap();
    sync(&forge, dir.path(), &location(), t0()).unwrap();
    let mut other = location();
    other.reference = "staging".to_string();
    assert!(read_cache(dir.path(), &other).unwrap().is_none());
}

#[test]
fn a_truncated_tree_is_refused() {
    struct Truncated;
    impl Transport for Truncated {
        fn get(&self, p: &str, _: Option<&str>, _: Option<&str>) -> Result<Reply> {
            let sha = "a".repeat(40);
            Ok(if p.contains("/commits/") {
                Reply {
                    status: 200,
                    etag: None,
                    body: sha,
                }
            } else {
                Reply {
                    status: 200,
                    etag: None,
                    body: r#"{"tree":[],"truncated":true}"#.to_string(),
                }
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let err = sync(&Truncated, dir.path(), &location(), t0()).unwrap_err();
    assert!(err.to_string().contains("truncated"));
}
