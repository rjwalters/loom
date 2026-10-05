//! `eta::fit::publish` tests (#10395): the envelope's checks, the round trip
//! captain → store → non-captain, and every refusal leaving the previous fit
//! in place. A fake in-memory store; no network.

use crate::eta::fit::coeffs::{self, CoefficientFile, FitMeta, FitWindow, Fitter};
use crate::eta::fit::publish::{
    self, envelope_for, fetch_and_install, publish_newest, read_status, FetchKind, PublishKind,
    Refusal, VerifyCtx, LATEST_PATH,
};
use crate::fleet_store::fetch::{Reply, Transport};
use crate::fleet_store::propose::WriteTransport;
use crate::fleet_store::StoreLocation;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

const CAPTAIN: &str = "loom-worker-1";

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, 0, 0, 0).unwrap()
}

fn fit_at(as_of: DateTime<Utc>) -> CoefficientFile {
    let meta = FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.0.0".to_string(),
            revision: "abc".to_string(),
        },
    };
    coeffs::fit(&meta, &[], &[])
}

fn loc() -> StoreLocation {
    StoreLocation {
        repo: "o/store".to_string(),
        reference: "eta-fit".to_string(),
    }
}

/// An in-memory contents API: path -> bytes on the `eta-fit` branch.
#[derive(Default)]
struct Store {
    files: RefCell<BTreeMap<String, Vec<u8>>>,
    branch: RefCell<bool>,
    writes: RefCell<Vec<String>>,
    fail_gets: RefCell<bool>,
}

impl Store {
    fn etag(&self, path: &str) -> Option<String> {
        self.files
            .borrow()
            .get(path)
            .map(|b| format!("\"{}\"", publish::sha256_hex(b)))
    }
}

fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        etag: None,
        body: body.into(),
    }
}

impl Transport for Store {
    fn get(
        &self,
        api_path: &str,
        accept: Option<&str>,
        etag: Option<&str>,
    ) -> anyhow::Result<Reply> {
        if *self.fail_gets.borrow() {
            anyhow::bail!("network down");
        }
        if api_path.contains("/git/ref/heads/") {
            return Ok(reply(if *self.branch.borrow() { 200 } else { 404 }, ""));
        }
        if api_path.contains("/commits/") {
            return Ok(reply(200, "a".repeat(40)));
        }
        let path = api_path
            .split("/contents/")
            .nth(1)
            .and_then(|p| p.split('?').next())
            .unwrap()
            .to_string();
        let files = self.files.borrow();
        let Some(bytes) = files.get(&path) else {
            return Ok(reply(404, ""));
        };
        let tag = self.etag(&path);
        if etag.is_some() && etag == tag.as_deref() {
            return Ok(reply(304, ""));
        }
        let body = if accept.is_some_and(|a| a.contains("raw")) {
            String::from_utf8(bytes.clone()).unwrap()
        } else {
            format!("{{\"sha\":\"{}\"}}", publish::sha256_hex(bytes))
        };
        Ok(Reply {
            status: 200,
            etag: tag,
            body,
        })
    }
}

impl WriteTransport for Store {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> anyhow::Result<Reply> {
        if api_path.ends_with("/git/refs") {
            *self.branch.borrow_mut() = true;
            return Ok(reply(201, ""));
        }
        assert_eq!(method, "PUT");
        let path = api_path.split("/contents/").nth(1).unwrap().to_string();
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let bytes = STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
        self.files.borrow_mut().insert(path.clone(), bytes);
        self.writes.borrow_mut().push(path);
        Ok(reply(201, ""))
    }
}

fn captain_publishes(store: &Store, root: &Path, fit: &CoefficientFile, now: DateTime<Utc>) {
    coeffs::write(
        &root
            .join(".loom/state/eta/fit")
            .join(coeffs::path_for(fit.as_of)),
        fit,
    )
    .unwrap();
    let kind = publish_newest(store, store, &loc(), "main", root, CAPTAIN, now);
    assert_eq!(kind, Some(PublishKind::Published));
}

fn fetch(store: &Store, root: &Path, captain: &str, now: DateTime<Utc>) -> FetchKind {
    fetch_and_install(store, &loc(), root, captain, now, Duration::days(3))
}

fn installed_ids(root: &Path) -> Vec<String> {
    let mut v: Vec<_> = std::fs::read_dir(coeffs::fit_dir(root))
        .map(|d| d.filter_map(Result::ok).collect())
        .unwrap_or_default();
    v.sort_by_key(std::fs::DirEntry::path);
    v.iter()
        .filter_map(|e| coeffs::read(&e.path()))
        .map(|f| f.id)
        .collect()
}

#[test]
fn publish_then_fetch_installs_the_same_fit_and_is_idempotent() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let fit = fit_at(at(4));
    let now = at(4) + Duration::hours(8);
    captain_publishes(&store, cap.path(), &fit, now);
    assert!(store.files.borrow().contains_key(LATEST_PATH));
    assert!(*store.branch.borrow());
    // The fit file first, the envelope last, so the envelope never names a
    // file that is not there yet.
    assert_eq!(
        *store.writes.borrow(),
        vec![format!("eta/fit/{}.json", fit.id), LATEST_PATH.to_string()]
    );

    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Installed);
    assert_eq!(installed_ids(host.path()), vec![fit.id.clone()]);
    // The same bytes, so the same estimates (determinism).
    let a = std::fs::read(
        cap.path()
            .join(".loom/state/eta/fit")
            .join(coeffs::path_for(fit.as_of)),
    )
    .unwrap();
    let b = std::fs::read(coeffs::fit_dir(host.path()).join(coeffs::path_for(fit.as_of))).unwrap();
    assert_eq!(a, b);
    let status = read_status(host.path());
    assert_eq!(status.fit_id.as_deref(), Some(fit.id.as_str()));
    assert_eq!(status.captain_host.as_deref(), Some(CAPTAIN));

    // A second fetch is a 304; the captain's second publish writes nothing.
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::NotModified);
    let writes = store.writes.borrow().len();
    std::fs::remove_file(publish::status_path(cap.path())).unwrap();
    let again = publish_newest(&store, &store, &loc(), "main", cap.path(), CAPTAIN, now);
    assert_eq!(again, Some(PublishKind::AlreadyPublished));
    assert_eq!(store.writes.borrow().len(), writes);
}

fn tamper(store: &Store, f: impl FnOnce(&mut Vec<u8>)) {
    let key = store
        .files
        .borrow()
        .keys()
        .find(|k| k.as_str() != LATEST_PATH)
        .unwrap()
        .clone();
    f(store.files.borrow_mut().get_mut(&key).unwrap());
}

#[test]
fn tampered_or_partial_files_are_refused_and_the_previous_fit_stays() {
    for case in ["flip", "truncate"] {
        let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Store::default();
        let now = at(5) + Duration::hours(8);
        // A previous, older fit is already installed.
        let old = fit_at(at(3));
        coeffs::write(&coeffs::fit_dir(host.path()).join(coeffs::path_for(old.as_of)), &old)
            .unwrap();
        captain_publishes(&store, cap.path(), &fit_at(at(5)), now);
        tamper(&store, |b| {
            if case == "flip" {
                let i = b.iter().position(|c| *c == b'a').unwrap();
                b[i] = b'b';
            } else {
                b.truncate(b.len() / 2);
            }
        });
        assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Refused, "{case}");
        assert_eq!(installed_ids(host.path()), vec![old.id.clone()], "{case}");
        assert_eq!(read_status(host.path()).reason.as_deref(), Some("sha_mismatch"));
    }
}

fn ctx(now: DateTime<Utc>, local: Option<DateTime<Utc>>) -> VerifyCtx<'static> {
    VerifyCtx {
        captain: CAPTAIN,
        now,
        local_as_of: local,
        max_age: Duration::days(3),
    }
}

#[test]
fn verify_refusals() {
    let fit = fit_at(at(4));
    let bytes = coeffs::to_json(&fit).into_bytes();
    let now = at(4) + Duration::hours(8);
    let env = envelope_for(&fit, &bytes, CAPTAIN, now);
    assert!(publish::verify(&env, &bytes, &ctx(now, None)).is_ok());
    assert!(publish::verify(&env, &bytes, &ctx(now, Some(at(4)))).is_ok());

    let code = |env: &publish::Envelope, bytes: &[u8], c: &VerifyCtx<'_>| {
        publish::verify(env, bytes, c).unwrap_err().code()
    };
    assert_eq!(code(&env, &bytes[..bytes.len() - 5], &ctx(now, None)), "sha_mismatch");
    let mut e = env.clone();
    e.fit_id = "0123456789abcdef".to_string();
    assert_eq!(code(&e, &bytes, &ctx(now, None)), "envelope_mismatch");
    let mut e = env.clone();
    e.captain_host = "someone-else".to_string();
    assert_eq!(code(&e, &bytes, &ctx(now, None)), "wrong_captain");
    assert_eq!(code(&env, &bytes, &ctx(at(4) - Duration::hours(1), None)), "future_as_of");
    assert_eq!(code(&env, &bytes, &ctx(at(9), None)), "stale");
    assert_eq!(code(&env, &bytes, &ctx(now, Some(at(5)))), "older_than_local");
    // Not an eta-fit/v1 file, even with a matching sha.
    let junk = b"{\"schema\":\"eta-fit/v9\"}".to_vec();
    let mut e = env.clone();
    e.sha256 = publish::sha256_hex(&junk);
    assert_eq!(code(&e, &junk, &ctx(now, None)), "bad_fit");
}

#[test]
fn envelope_schema_and_file_name_are_checked() {
    let fit = fit_at(at(4));
    let bytes = coeffs::to_json(&fit).into_bytes();
    let env = envelope_for(&fit, &bytes, CAPTAIN, at(4));
    let parse = |e: &publish::Envelope| publish::parse_envelope(&serde_json::to_vec(e).unwrap());
    assert!(parse(&env).is_ok());
    let mut e = env.clone();
    e.schema = "eta-fit-pub/v2".to_string();
    assert!(matches!(parse(&e), Err(Refusal::BadEnvelope(_))));
    for bad in [
        "../x.json",
        "a/b.json",
        "0123456789abcdef",
        "0123456789ABCDEF.json",
        "..json",
    ] {
        let mut e = env.clone();
        e.file = bad.to_string();
        assert!(matches!(parse(&e), Err(Refusal::BadFileName(_))), "{bad}");
    }
    assert!(matches!(publish::parse_envelope(b"nope"), Err(Refusal::BadEnvelope(_))));
}

#[test]
fn a_former_captains_publication_is_refused_after_a_captain_change() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let now = at(4) + Duration::hours(8);
    captain_publishes(&store, cap.path(), &fit_at(at(4)), now);
    assert_eq!(fetch(&store, host.path(), "loom-worker-2", now), FetchKind::Refused);
    assert!(installed_ids(host.path()).is_empty());
}

#[test]
fn a_304_is_trusted_only_while_the_captain_and_installed_fit_still_match() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let now = at(4) + Duration::hours(8);
    let fit = fit_at(at(4));
    captain_publishes(&store, cap.path(), &fit, now);
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Installed);
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::NotModified);

    // The installed fit went missing: no 304, the fit is fetched and installed again.
    std::fs::remove_file(coeffs::fit_dir(host.path()).join(coeffs::path_for(fit.as_of))).unwrap();
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Installed);
    assert_eq!(installed_ids(host.path()), vec![fit.id.clone()]);

    // The declared captain changed while the old envelope is unchanged: the
    // former captain's publication is re-verified and refused, not served.
    assert_eq!(fetch(&store, host.path(), "loom-worker-2", now), FetchKind::Refused);
    assert_eq!(read_status(host.path()).reason.as_deref(), Some("wrong_captain"));
}

#[test]
fn the_captain_never_publishes_to_the_reviewed_branch() {
    let cap = tempfile::tempdir().unwrap();
    let store = Store::default();
    let fit = fit_at(at(4));
    coeffs::write(&coeffs::fit_dir(cap.path()).join(coeffs::path_for(fit.as_of)), &fit).unwrap();
    for (reference, base) in [("main", "release"), ("stable", "stable")] {
        let loc = StoreLocation {
            repo: "o/store".to_string(),
            reference: reference.to_string(),
        };
        let r = publish::publish(&store, &store, &loc, base, &fit, CAPTAIN, at(4));
        assert!(r.unwrap_err().to_string().contains("dedicated"), "{reference}");
    }
    assert!(store.writes.borrow().is_empty());
    assert!(!*store.branch.borrow());
}

#[test]
fn absent_error_and_stale_publications_fall_back() {
    let host = tempfile::tempdir().unwrap();
    let store = Store::default();
    let now = at(4) + Duration::hours(8);
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Absent);
    assert!(!FetchKind::Absent.serving_published());

    *store.fail_gets.borrow_mut() = true;
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::FetchError);
    assert!(!FetchKind::FetchError.serving_published());
    *store.fail_gets.borrow_mut() = false;

    let cap = tempfile::tempdir().unwrap();
    captain_publishes(&store, cap.path(), &fit_at(at(4)), now);
    assert_eq!(fetch(&store, host.path(), CAPTAIN, at(9)), FetchKind::Stale);
    assert!(installed_ids(host.path()).is_empty());
    assert_eq!(read_status(host.path()).kind, Some(FetchKind::Stale));
}

#[test]
fn an_older_publication_does_not_replace_a_newer_local_fit() {
    let (cap, host) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::default();
    let now = at(5) + Duration::hours(8);
    captain_publishes(&store, cap.path(), &fit_at(at(4)), now);
    let newer = fit_at(at(5));
    coeffs::write(&coeffs::fit_dir(host.path()).join(coeffs::path_for(newer.as_of)), &newer)
        .unwrap();
    assert_eq!(fetch(&store, host.path(), CAPTAIN, now), FetchKind::Refused);
    assert_eq!(installed_ids(host.path()), vec![newer.id]);
}

#[test]
fn a_publish_failure_is_recorded_not_fatal() {
    struct Deny(Store);
    impl Transport for Deny {
        fn get(&self, p: &str, a: Option<&str>, e: Option<&str>) -> anyhow::Result<Reply> {
            self.0.get(p, a, e)
        }
    }
    impl WriteTransport for Deny {
        fn write(&self, _: &str, _: &str, _: &Value) -> anyhow::Result<Reply> {
            Ok(reply(403, "forbidden"))
        }
    }
    let cap = tempfile::tempdir().unwrap();
    let store = Deny(Store::default());
    let fit = fit_at(at(4));
    coeffs::write(&coeffs::fit_dir(cap.path()).join(coeffs::path_for(fit.as_of)), &fit).unwrap();
    let r = publish_newest(&store, &store, &loc(), "main", cap.path(), CAPTAIN, at(4));
    assert_eq!(r, None);
    let status = read_status(cap.path());
    assert!(status.publish_error.unwrap().contains("contents:write"));
    assert_eq!(status.last_published_id, None);
}

#[test]
fn config_defaults() {
    let empty = serde_json::json!({});
    assert_eq!(publish::resolve_ref(&empty), "eta-fit");
    assert_eq!(publish::resolve_max_age(&empty), Duration::days(3));
    let set = serde_json::json!({"fleet": {"etaFitRef": "x/y", "etaFitMaxAgeDays": 7}});
    assert_eq!(publish::resolve_ref(&set), "x/y");
    assert_eq!(publish::resolve_max_age(&set), Duration::days(7));
}

#[test]
fn without_fleet_repo_the_feature_is_off() {
    // The default (no fleet.repo anywhere): nothing is fetched or published,
    // so every host behaves as before #10395. `LOOM_FLEET_REPO` is not set in
    // the test environment.
    if std::env::var(crate::fleet_store::FLEET_REPO_ENV).is_ok() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    assert!(publish::publication_location(root.path()).is_none());
    assert!(
        crate::observability::eta_fleet_refresh::distribute_fetch(root.path(), CAPTAIN, at(4))
            .is_none()
    );
}
