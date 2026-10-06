//! The fleet store's `repos.yml` history reader (#10508, #10586): the forge
//! listing, the content cache, the observation convention, and what an
//! unavailable history loads as.

use super::fit_rows::{h, OTHER, REPO};
use crate::eta::fit::features_v2::N_FEATURES_V2;
use crate::eta::repo_priority::{repo_rank, revision_at, KnowBasis};
use crate::eta::roster_history::{
    load, sync, HistoryStatus, MAX_PAGES, MAX_STALE_HOURS, PER_PAGE, WINDOW_DAYS,
};
use crate::fleet_store::fetch::{Reply, Transport};
use crate::fleet_store::StoreLocation;
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use std::cell::{Cell, RefCell};

/// A fake store: `repos.yml`'s commits, oldest first, answering the three
/// requests the reader makes.
pub(crate) struct FakeStore {
    /// `(sha, committer date, repos.yml)`, in history order.
    pub(crate) commits: RefCell<Vec<(String, DateTime<Utc>, String)>>,
    pub(crate) offline: Cell<bool>,
    pub(crate) calls: RefCell<Vec<String>>,
}

pub(crate) fn location() -> StoreLocation {
    StoreLocation {
        repo: "acme/fleet".to_string(),
        reference: "main".to_string(),
    }
}

/// A `repos.yml` whose fleet is `REPO` and `OTHER` at these priorities.
pub(crate) fn roster_yaml(repo: u32, other: u32) -> String {
    format!(
        "root: /srv\nrepos:\n  - name: loom\n    remote: git@github.com:{REPO}.git\n    \
         fleet: true\n    fleet_priority: {repo}\n  - name: other\n    \
         remote: https://github.com/{OTHER}.git\n    fleet: true\n    fleet_priority: {other}\n"
    )
}

impl FakeStore {
    pub(crate) fn new() -> Self {
        Self {
            commits: RefCell::new(Vec::new()),
            offline: Cell::new(false),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Push a commit dated `at` (which may be backdated).
    pub(crate) fn commit(&self, at: DateTime<Utc>, yaml: &str) -> String {
        let n = self.commits.borrow().len();
        let sha = format!("{:040x}", n + 1);
        self.commits
            .borrow_mut()
            .push((sha.clone(), at, yaml.to_string()));
        sha
    }

    fn listing(&self, picked: Vec<(String, DateTime<Utc>)>) -> Reply {
        let items: Vec<serde_json::Value> = picked
            .iter()
            .map(|(sha, at)| {
                serde_json::json!({"sha": sha, "commit": {"committer": {"date": at.to_rfc3339()}}})
            })
            .collect();
        Reply {
            status: 200,
            etag: None,
            body: serde_json::Value::Array(items).to_string(),
        }
    }
}

fn param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    path.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn time(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

impl Transport for FakeStore {
    fn get(&self, api_path: &str, _accept: Option<&str>, _etag: Option<&str>) -> Result<Reply> {
        self.calls.borrow_mut().push(api_path.to_string());
        if self.offline.get() {
            bail!("network is unreachable");
        }
        // The forge lists newest first: here, reverse history order.
        let newest_first: Vec<(String, DateTime<Utc>)> = self
            .commits
            .borrow()
            .iter()
            .rev()
            .map(|(s, at, _)| (s.clone(), *at))
            .collect();
        // One linear history, whatever ref is asked for.
        if api_path.starts_with("repos/acme/fleet/commits?path=repos.yml&sha=") {
            if let Some(until) = param(api_path, "until") {
                let until = time(until);
                return Ok(self.listing(
                    newest_first
                        .into_iter()
                        .filter(|(_, at)| *at <= until)
                        .take(1)
                        .collect(),
                ));
            }
            let since = time(param(api_path, "since").unwrap());
            let per: usize = param(api_path, "per_page").unwrap().parse().unwrap();
            let page: usize = param(api_path, "page").unwrap().parse().unwrap();
            return Ok(self.listing(
                newest_first
                    .into_iter()
                    .filter(|(_, at)| *at >= since)
                    .skip((page - 1) * per)
                    .take(per)
                    .collect(),
            ));
        }
        if let Some(sha) = api_path.strip_prefix("repos/acme/fleet/contents/repos.yml?ref=") {
            let commits = self.commits.borrow();
            let Some((_, _, yaml)) = commits.iter().find(|(s, ..)| s == sha) else {
                return Ok(Reply {
                    status: 404,
                    etag: None,
                    body: "{}".to_string(),
                });
            };
            use base64::{engine::general_purpose, Engine as _};
            use sha2::{Digest, Sha256};
            let blob = hex::encode(Sha256::digest(yaml.as_bytes()))[..40].to_string();
            let body = serde_json::json!({
                "sha": blob,
                "encoding": "base64",
                "content": general_purpose::STANDARD.encode(yaml.as_bytes()),
            });
            return Ok(Reply {
                status: 200,
                etag: None,
                body: body.to_string(),
            });
        }
        bail!("unexpected request {api_path}")
    }
}

/// The fixture's `now`: the history starts at `h(0)`.
fn now() -> DateTime<Utc> {
    h(20.0)
}

#[test]
fn the_first_poll_records_no_observations_and_loads_in_history_order() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    store.commit(h(5.0), &roster_yaml(100, 10));
    let report = sync(&store, tmp.path(), &location(), now()).unwrap();
    assert_eq!((report.revisions, report.new, report.fetched), (2, 2, 2));

    let (history, coverage) = load(tmp.path(), now());
    let history = history.expect("loaded");
    assert_eq!(coverage.status, HistoryStatus::Loaded);
    assert_eq!((coverage.revisions, coverage.observed, coverage.commit_date_only), (2, 0, 2));
    assert!(history.iter().all(|r| r.basis() == KnowBasis::CommitDate));
    assert_eq!(history[0].committed_at, h(0.0));
    let at = |t| revision_at(&history, t).map(|r| repo_rank(r, OTHER));
    assert_eq!(at(h(1.0)), Some(Some(0.5)), "tied at first");
    assert_eq!(at(h(6.0)), Some(Some(0.0)), "OTHER moved to the front at 5 h");
    assert_eq!(at(h(0.0)), None, "nothing knowable strictly before the first commit");
}

#[test]
fn a_commit_first_seen_on_a_later_poll_counts_from_that_poll() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    sync(&store, tmp.path(), &location(), h(10.0)).unwrap();
    // Backdated: committed at 2 h, first on the ref between the 10 h and
    // 30 h polls.
    store.commit(h(2.0), &roster_yaml(0, 100));
    let report = sync(&store, tmp.path(), &location(), h(30.0)).unwrap();
    assert_eq!((report.new, report.fetched), (1, 1), "only the new commit is fetched");

    let (history, coverage) = load(tmp.path(), h(30.0));
    let history = history.unwrap();
    assert_eq!((coverage.observed, coverage.commit_date_only), (1, 1));
    let rank = |t| revision_at(&history, t).and_then(|r| repo_rank(r, REPO));
    assert_eq!(rank(h(20.0)), Some(0.5), "not read before it was observed");
    assert_eq!(rank(h(30.0)), Some(0.5), "nor at the observation itself");
    assert_eq!(rank(h(31.0)), Some(0.0), "read once observed");
    // A third poll keeps the recorded observation, and fetches nothing.
    let again = sync(&store, tmp.path(), &location(), h(40.0)).unwrap();
    assert_eq!((again.new, again.fetched), (0, 0));
    assert_eq!(load(tmp.path(), h(40.0)).0.unwrap(), history);
}

#[test]
fn the_window_keeps_an_anchor_for_the_revision_in_force_when_it_opens() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    let now = h(0.0) + Duration::days(40);
    store.commit(h(0.0), &roster_yaml(100, 100));
    store.commit(h(1.0), &roster_yaml(1, 100));
    store.commit(now - Duration::days(2), &roster_yaml(100, 1));
    let report = sync(&store, tmp.path(), &location(), now).unwrap();
    assert_eq!(report.revisions, 2, "the newest pre-window commit, and the window");
    let history = load(tmp.path(), now).0.unwrap();
    let opens = now - Duration::days(WINDOW_DAYS);
    assert_eq!(revision_at(&history, opens).and_then(|r| repo_rank(r, REPO)), Some(0.0));
}

#[test]
fn a_long_window_is_paginated_and_an_overlong_one_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    let now = h(0.0) + Duration::days(5);
    for i in 0..=PER_PAGE {
        store.commit(h(i as f64), &roster_yaml(100, i as u32));
    }
    let report = sync(&store, tmp.path(), &location(), now).unwrap();
    assert_eq!(report.revisions, PER_PAGE + 1);
    assert!(store.calls.borrow().iter().any(|c| c.ends_with("&page=2")));

    let too_many = FakeStore::new();
    for i in 0..(MAX_PAGES * PER_PAGE) {
        too_many.commit(h(0.0) + Duration::minutes(i as i64), &roster_yaml(1, 2));
    }
    let tmp2 = tempfile::tempdir().unwrap();
    assert!(sync(&too_many, tmp2.path(), &location(), now).is_err());
    assert_eq!(load(tmp2.path(), now).1.status, HistoryStatus::Missing);
}

#[test]
fn a_failed_poll_leaves_the_cache_and_moves_no_observation_earlier() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    sync(&store, tmp.path(), &location(), h(10.0)).unwrap();
    let before = std::fs::read(tmp.path().join("index.json")).unwrap();
    store.commit(h(11.0), &roster_yaml(5, 100));
    store.offline.set(true);
    assert!(sync(&store, tmp.path(), &location(), h(12.0)).is_err());
    assert_eq!(std::fs::read(tmp.path().join("index.json")).unwrap(), before);
    store.offline.set(false);
    sync(&store, tmp.path(), &location(), h(15.0)).unwrap();
    let history = load(tmp.path(), h(15.0)).0.unwrap();
    assert_eq!(history[1].observed_at, Some(h(15.0)), "the successful poll, not the failed one");
}

#[test]
fn unavailable_history_loads_as_unknown_with_its_reason() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(load(tmp.path(), now()), (None, Default::default()));
    assert_eq!(load(tmp.path(), now()).1.status, HistoryStatus::Missing);

    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    sync(&store, tmp.path(), &location(), h(1.0)).unwrap();
    let late = h(1.0) + Duration::hours(MAX_STALE_HOURS) + Duration::seconds(1);
    let (history, coverage) = load(tmp.path(), late);
    assert_eq!((history, coverage.status), (None, HistoryStatus::Stale));
    assert!(load(tmp.path(), h(2.0)).0.is_some());

    // A corrupted revision is a gap: the whole history is unknown.
    for entry in std::fs::read_dir(tmp.path().join("blobs")).unwrap() {
        std::fs::write(entry.unwrap().path(), "repos: not-a-list\n").unwrap();
    }
    let (history, coverage) = load(tmp.path(), h(2.0));
    assert_eq!((history, coverage.status), (None, HistoryStatus::Unreadable));
}

// ---- The leak test through the reader (#10586) -----------------------------

/// What the v2 fit sees and writes, for the priority-input fixture fitted at
/// the cutoff with whatever history the cache in `dir` loads as of `at`:
/// every row's `eta-fit/v2` feature vector, and the v2 file's bytes.
///
/// The fixture is too small to fit coefficients (its stages are below the
/// minimum rows), so the file alone would not react to the features: the
/// vectors carry the sensitivity, and the positive controls assert on them.
fn fitted_v2(dir: &std::path::Path, at: DateTime<Utc>) -> (Vec<[f64; N_FEATURES_V2]>, String) {
    use super::fit_rows::cutoff;
    use super::priority_inputs::{specs, trained};
    use crate::eta::fit::{coeffs, run, v2, Fitter};
    let (history, _) = load(dir, at);
    let assembled = trained(&specs(), history.as_deref());
    let fitter = Fitter {
        version: "0.19.0".to_string(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
    };
    (
        v2::features_v2(&assembled.rows, &assembled.priority_inputs),
        coeffs::to_json(&run::fit_v2_of(&assembled, cutoff(), &fitter)),
    )
}

/// The baseline store, polled once at 6 h: the 0 h and 5 h revisions.
fn polled_baseline(dir: &std::path::Path) -> FakeStore {
    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    store.commit(h(5.0), &roster_yaml(100, 10));
    sync(&store, dir, &location(), h(6.0)).unwrap();
    store
}

/// The leak test through the real reader (#10586). A `repos.yml` edit
/// committed at 6.5 h but first listed by a poll after the cutoff
/// (backdated), or committed after the cutoff, leaves every v2 feature
/// vector and the v2 file byte-identical; an unavailable history (no cache,
/// or a stale one) is exactly no history. Positive controls: the baseline
/// history does move the vectors, and so does the same 6.5 h commit first
/// listed by a 7 h poll, so the test cannot pass vacuously.
#[test]
fn roster_edits_knowable_only_after_the_cutoff_leave_the_v2_fit_byte_identical() {
    use super::fit_rows::cutoff;
    let edit = roster_yaml(0, 500);

    let base_dir = tempfile::tempdir().unwrap();
    polled_baseline(base_dir.path());
    let base = fitted_v2(base_dir.path(), h(6.0));

    // Backdated: committed before the cutoff, first seen after it.
    let backdated = tempfile::tempdir().unwrap();
    let store = polled_baseline(backdated.path());
    store.commit(h(6.5), &edit);
    let seen = cutoff() + Duration::hours(1);
    sync(&store, backdated.path(), &location(), seen).unwrap();
    assert_eq!(load(backdated.path(), seen).1.observed, 1);
    assert_eq!(fitted_v2(backdated.path(), seen), base, "backdated commit");

    // Committed after the cutoff.
    let after = tempfile::tempdir().unwrap();
    let store = polled_baseline(after.path());
    store.commit(cutoff() + Duration::hours(1), &edit);
    let seen = cutoff() + Duration::hours(2);
    sync(&store, after.path(), &location(), seen).unwrap();
    assert_eq!(load(after.path(), seen).1.revisions, 3);
    assert_eq!(fitted_v2(after.path(), seen), base, "post-cutoff commit");

    // Unavailable: no cache, or a stale one, is no history at all.
    let missing = tempfile::tempdir().unwrap();
    let none = fitted_v2(missing.path(), h(6.0));
    let stale = fitted_v2(base_dir.path(), h(6.0) + Duration::hours(MAX_STALE_HOURS + 1));
    assert_eq!(stale, none, "a stale cache reads as no history");
    assert_ne!(base.0, none.0, "the baseline history is actually read");

    // The same commit, first seen at 7 h, before the rows that read it.
    let early = tempfile::tempdir().unwrap();
    let store = polled_baseline(early.path());
    store.commit(h(6.5), &edit);
    sync(&store, early.path(), &location(), h(7.0)).unwrap();
    assert_ne!(
        fitted_v2(early.path(), h(7.0)).0,
        base.0,
        "an edit observed early moves the rows"
    );
}

#[test]
fn a_cache_of_another_store_offers_no_observations() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FakeStore::new();
    store.commit(h(0.0), &roster_yaml(100, 100));
    let other = StoreLocation {
        repo: "acme/fleet".to_string(),
        reference: "staging".to_string(),
    };
    // A poll of another ref, then a first poll of `main`: its commits were
    // never listed by a poll of `main`, so they carry no observation.
    sync(&store, tmp.path(), &other, h(1.0)).unwrap();
    store.commit(h(1.5), &roster_yaml(1, 100));
    sync(&store, tmp.path(), &location(), h(2.0)).unwrap();
    let history = load(tmp.path(), h(2.0)).0.unwrap();
    assert!(history.iter().all(|r| r.basis() == KnowBasis::CommitDate));
}
