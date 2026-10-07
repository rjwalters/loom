//! `eta::fit::publish_v2` tests (#10508, item 3 of #10586): the captain's
//! `eta-fit/v2` file reaches a non-captain host's `<fit_dir>/v2`, where
//! `Registry::load` picks it up, while v1's publication is untouched and
//! every v2 failure keeps v1 (and the previous v2 file) as it was.

use super::fit_publish::{at, fit_at, loc, Store, CAPTAIN};
use super::keen_wren::v2_fixture;
use crate::eta::fit::coeffs::{self, CoefficientFile};
use crate::eta::fit::publish::{
    self, publish_newest, read_status, FetchKind, PublishKind, LATEST_PATH,
};
use crate::eta::fit::publish_v2::{
    fetch_and_install_v2, publish_newest_v2, publish_v2, read_status_v2, LATEST_PATH_V2, V2_DIR,
};
use crate::eta::fit::v2::{fit_dir_v2, load_latest_v2};
use crate::eta::Registry;
use chrono::{DateTime, Duration, Utc};
use std::path::Path;

fn write_local_v2(root: &Path, fit: &CoefficientFile) {
    coeffs::write(&fit_dir_v2(root).join(coeffs::path_for(fit.as_of)), fit).unwrap();
}

fn write_local_v1(root: &Path, fit: &CoefficientFile) {
    coeffs::write(&coeffs::fit_dir(root).join(coeffs::path_for(fit.as_of)), fit).unwrap();
}

fn fetch_v2(store: &Store, root: &Path, captain: &str, now: DateTime<Utc>) -> FetchKind {
    fetch_and_install_v2(store, &loc(), root, captain, now, Duration::days(3))
}

fn captain_publishes_v2(store: &Store, root: &Path, fit: &CoefficientFile, now: DateTime<Utc>) {
    write_local_v2(root, fit);
    let kind = publish_newest_v2(store, store, &loc(), "main", root, CAPTAIN, now);
    assert_eq!(kind, Some(PublishKind::Published));
}

#[test]
fn a_non_captain_installs_the_published_v2_file_and_the_registry_picks_it_up() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let v1 = fit_at(at(4));
    let v2 = v2_fixture(at(4), 0.0, 0.0);
    let now = at(4) + Duration::hours(8);
    write_local_v1(cap.path(), &v1);
    assert_eq!(
        publish_newest(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now),
        Some(PublishKind::Published)
    );
    captain_publishes_v2(&store, cap.path(), &v2, now);
    // The file first, the envelope last, in the v2 lane; v1's paths are
    // exactly what they were.
    let writes = store.writes.borrow().clone();
    assert_eq!(
        writes,
        vec![
            format!("eta/fit/{}.json", v1.id),
            LATEST_PATH.to_string(),
            format!("{V2_DIR}/{}.json", v2.id),
            LATEST_PATH_V2.to_string(),
        ]
    );

    // The non-captain host has nothing: keen-wren would be `no_model`.
    assert!(Registry::load(host.path(), now).fit_v2().is_none());
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Installed);
    let installed = fit_dir_v2(host.path()).join(coeffs::path_for(v2.as_of));
    let a = std::fs::read(fit_dir_v2(cap.path()).join(coeffs::path_for(v2.as_of))).unwrap();
    assert_eq!(std::fs::read(installed).unwrap(), a, "byte for byte");
    let registry = Registry::load(host.path(), now);
    assert_eq!(registry.fit_v2_id(), Some(v2.id.as_str()));
    assert!(registry.fit().is_none(), "v1 was not fetched by the v2 lane");
    assert!(coeffs::load_latest(host.path(), now).is_none());

    // v2's outcome is its own status; v1's is untouched.
    let status = read_status_v2(host.path());
    assert_eq!(status.fit_id.as_deref(), Some(v2.id.as_str()));
    assert_eq!(status.kind, Some(FetchKind::Installed));
    assert_eq!(read_status(host.path()), publish::PubStatus::default());

    // Steady state: a 304; the captain's republish writes nothing.
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::NotModified);
    let n = store.writes.borrow().len();
    std::fs::remove_file(crate::eta::fit::publish_v2::status_path_v2(cap.path())).unwrap();
    assert_eq!(
        publish_newest_v2(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now),
        Some(PublishKind::AlreadyPublished)
    );
    assert_eq!(store.writes.borrow().len(), n);
}

#[test]
fn no_v2_file_leaves_v1_publication_and_fetch_unchanged() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let v1 = fit_at(at(4));
    let now = at(4) + Duration::hours(8);
    write_local_v1(cap.path(), &v1);
    // Captain with no v2 file: nothing is written for the v2 lane.
    assert_eq!(
        publish_newest_v2(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now),
        None
    );
    assert!(store.writes.borrow().is_empty());
    assert_eq!(
        publish_newest(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now),
        Some(PublishKind::Published)
    );
    assert_eq!(
        *store.writes.borrow(),
        vec![format!("eta/fit/{}.json", v1.id), LATEST_PATH.to_string()]
    );
    // A host fetching the v2 lane from a v1-only store: absent, nothing
    // installed, and v1's own fetch still installs.
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Absent);
    assert!(!fit_dir_v2(host.path()).exists());
    assert_eq!(
        publish::fetch_and_install(&store, &loc(), host.path(), CAPTAIN, now, Duration::days(3)),
        FetchKind::Installed
    );
    let registry = Registry::load(host.path(), now);
    assert_eq!(registry.fit_id(), Some(v1.id.as_str()));
    assert!(registry.fit_v2().is_none());
}

#[test]
fn every_v2_refusal_keeps_the_previous_state() {
    let store = Store::default();
    let cap = tempfile::tempdir().unwrap();
    let v2 = v2_fixture(at(4), 0.0, 0.0);
    let now = at(4) + Duration::hours(8);
    captain_publishes_v2(&store, cap.path(), &v2, now);
    let host = tempfile::tempdir().unwrap();

    // Not the declared captain.
    assert_eq!(fetch_v2(&store, host.path(), "someone-else", now), FetchKind::Refused);
    assert_eq!(read_status_v2(host.path()).reason.as_deref(), Some("wrong_captain"));
    assert!(load_latest_v2(host.path(), now).is_none());
    // Stale.
    let late = at(4) + Duration::days(5);
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, late), FetchKind::Stale);
    assert!(load_latest_v2(host.path(), late).is_none());
    // Tampered bytes.
    let key = format!("{V2_DIR}/{}.json", v2.id);
    store.files.borrow_mut().get_mut(&key).unwrap().push(b' ');
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Refused);
    assert_eq!(read_status_v2(host.path()).reason.as_deref(), Some("sha_mismatch"));
    assert!(load_latest_v2(host.path(), now).is_none());
    // Store unreadable.
    *store.fail_gets.borrow_mut() = true;
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::FetchError);
    assert!(load_latest_v2(host.path(), now).is_none());
}

#[test]
fn a_v1_file_on_the_v2_lane_is_refused_and_never_published_there() {
    let store = Store::default();
    let v1 = fit_at(at(4));
    let now = at(4) + Duration::hours(8);
    let err = publish_v2(&store, &store, &loc(), "main", &v1, CAPTAIN, now).unwrap_err();
    assert!(format!("{err:#}").contains("v2 lane"), "{err:#}");
    assert!(store.writes.borrow().is_empty());

    // A v1 file smuggled under the v2 lane's names is refused as `bad_fit`.
    let bytes = coeffs::to_json(&v1).into_bytes();
    let env = publish::envelope_for(&v1, &bytes, CAPTAIN, now);
    store
        .files
        .borrow_mut()
        .insert(LATEST_PATH_V2.to_string(), serde_json::to_vec_pretty(&env).unwrap());
    store
        .files
        .borrow_mut()
        .insert(format!("{V2_DIR}/{}.json", v1.id), bytes);
    let host = tempfile::tempdir().unwrap();
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Refused);
    assert_eq!(read_status_v2(host.path()).reason.as_deref(), Some("bad_fit"));
    assert!(!fit_dir_v2(host.path()).exists());
}

#[test]
fn v2_publication_refuses_the_reviewed_branch() {
    let store = Store::default();
    let v2 = v2_fixture(at(4), 0.0, 0.0);
    let mut main = loc();
    main.reference = "main".to_string();
    let err = publish_v2(&store, &store, &main, "stable", &v2, CAPTAIN, at(4)).unwrap_err();
    assert!(format!("{err:#}").contains("refusing to publish"), "{err:#}");
    let mut reviewed = loc();
    reviewed.reference = "stable".to_string();
    assert!(publish_v2(&store, &store, &reviewed, "stable", &v2, CAPTAIN, at(4)).is_err());
    assert!(store.writes.borrow().is_empty());
}

#[test]
fn a_broken_v2_lane_leaves_v1_fetch_and_status_unchanged() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let v1 = fit_at(at(4));
    let v2 = v2_fixture(at(4), 0.0, 0.0);
    let now = at(4) + Duration::hours(8);
    write_local_v1(cap.path(), &v1);
    publish_newest(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now).unwrap();
    captain_publishes_v2(&store, cap.path(), &v2, now);
    // The v2 envelope is garbage; v1's lane is intact.
    store
        .files
        .borrow_mut()
        .insert(LATEST_PATH_V2.to_string(), b"{not json".to_vec());
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Refused);
    assert_eq!(read_status_v2(host.path()).reason.as_deref(), Some("bad_envelope"));
    assert_eq!(read_status(host.path()), publish::PubStatus::default());
    assert_eq!(
        publish::fetch_and_install(&store, &loc(), host.path(), CAPTAIN, now, Duration::days(3)),
        FetchKind::Installed
    );
    let registry = Registry::load(host.path(), now);
    assert_eq!(registry.fit_id(), Some(v1.id.as_str()));
    assert!(registry.fit_v2().is_none());
    // And v1's outcome never landed in v2's status.
    assert_eq!(read_status_v2(host.path()).kind, Some(FetchKind::Refused));
}

#[test]
fn a_captain_failover_republishes_the_v2_file_under_the_new_captain() {
    const B: &str = "loom-worker-2";
    let store = Store::default();
    let (a, b, c) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let v2 = v2_fixture(at(4), 0.0, 0.0);
    let now = at(4) + Duration::hours(8);
    captain_publishes_v2(&store, a.path(), &v2, now);
    assert_eq!(fetch_v2(&store, b.path(), CAPTAIN, now), FetchKind::Installed);
    // The captain switches to B: A's envelope is refused until B republishes.
    assert_eq!(fetch_v2(&store, c.path(), B, now), FetchKind::Refused);
    assert_eq!(read_status_v2(c.path()).reason.as_deref(), Some("wrong_captain"));
    // Same fit, same bytes, new captain: a publish, not a skip.
    assert_eq!(
        publish_newest_v2(&store, &store, &loc(), "main", b.path(), B, now),
        Some(PublishKind::Published)
    );
    let env: publish::Envelope =
        serde_json::from_slice(&store.files.borrow()[LATEST_PATH_V2]).unwrap();
    assert_eq!((env.captain_host.as_str(), env.fit_id.as_str()), (B, v2.id.as_str()));
    assert_eq!(fetch_v2(&store, c.path(), B, now), FetchKind::Installed);
    assert_eq!(load_latest_v2(c.path(), at(5)).map(|f| f.id), Some(v2.id));
}

#[test]
fn an_older_v2_publication_does_not_replace_a_newer_local_file() {
    let store = Store::default();
    let cap = tempfile::tempdir().unwrap();
    let old = v2_fixture(at(3), 0.0, 0.0);
    captain_publishes_v2(&store, cap.path(), &old, at(3) + Duration::hours(1));
    let host = tempfile::tempdir().unwrap();
    let newer = v2_fixture(at(4), 0.5, 0.0);
    write_local_v2(host.path(), &newer);
    let now = at(4) + Duration::hours(1);
    assert_eq!(fetch_v2(&store, host.path(), CAPTAIN, now), FetchKind::Refused);
    assert_eq!(read_status_v2(host.path()).reason.as_deref(), Some("older_than_local"));
    assert_eq!(load_latest_v2(host.path(), at(5)).map(|f| f.id), Some(newer.id));
}
