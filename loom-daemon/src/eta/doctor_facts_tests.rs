//! `eta::doctor_facts::gather` against a real directory (#10391). Kept
//! outside `doctor_facts.rs` so its read-only source test stays meaningful.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn snapshot(root: &Path, repo: &str, as_of: DateTime<Utc>) {
    let mut s = fleet::FleetSnapshot::empty(repo);
    s.merge(&[], as_of);
    fleet::write(&fleet::snapshot_path(root, repo), &s).unwrap();
}

/// Regression (#10407 review): the doctor is a CLI subcommand, so the
/// daemon's registered workspace root is absent. It must still see a fresh
/// reader published under its own root, not report `no_reader`.
#[test]
fn gather_resolves_a_fresh_reader_under_the_doctors_own_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    // Unique app id: the withdrawal registry is process-global.
    std::fs::write(
        root.join(".loom/config.json"),
        serde_json::json!({"forge": {"identities": {
            "readers": [{"appId": "eta-doctor-r1", "slug": "r1", "privateKeyPath": "/k/r1.pem"}]
        }}})
        .to_string(),
    )
    .unwrap();
    let roster = crate::forge_identity::resolve(root);
    assert_eq!(roster.readers.len(), 1);
    let dir = crate::forge_identity::reader_dir(root, "acme", &roster.readers[0]);
    crate::credential_preflight::publish_github_app_token(&dir, "ghs_test").unwrap();
    let side = crate::forge_identity::Sidecar {
        app_id: "eta-doctor-r1".into(),
        slug: Some("r1".into()),
        installation_id: "9".into(),
        expires_at: (Utc::now() + chrono::Duration::minutes(50)).to_rfc3339(),
    };
    std::fs::write(dir.join("identity.json"), serde_json::to_vec(&side).unwrap()).unwrap();

    let now = Utc::now();
    snapshot(root, "acme/alpha", now);
    snapshot(root, "other/beta", now);
    let facts = gather(root, "host-a", now);
    let by_repo: BTreeMap<&str, (bool, bool)> = facts
        .data
        .repos
        .iter()
        .map(|r| (r.repo.as_str(), (r.has_reader, r.unsupported_forge)))
        .collect();
    assert_eq!(by_repo.get("acme/alpha"), Some(&(true, false)), "{by_repo:?}");
    assert_eq!(
        by_repo.get("other/beta"),
        Some(&(false, false)),
        "no token for this owner: no_reader, not unsupported_forge"
    );
}

/// Write a minimal coefficient file cut off at `as_of`; returns its id.
fn fit_file(root: &Path, as_of: DateTime<Utc>) -> String {
    let meta = fit::FitMeta {
        as_of,
        window: fit::FitWindow::standard(as_of),
        fitter: fit::Fitter {
            version: "0.0.0".to_string(),
            revision: "0".repeat(40),
        },
    };
    let file = fit::CoefficientFile::empty(&meta).with_derived_id();
    let path = fit::coeffs::fit_dir(root).join(fit::coeffs::path_for(as_of));
    fit::coeffs::write(&path, &file).unwrap();
    file.id
}

fn status_of(facts: &Facts, link: &str, check: &str) -> super::super::doctor::Status {
    super::super::doctor::evaluate(facts)
        .into_iter()
        .find(|c| c.link == link && c.check == check)
        .unwrap_or_else(|| panic!("no {link}.{check} check"))
        .status
}

/// Regression (#10407 review): a fit cut off after `now` is one the
/// estimator cannot serve yet (`fit::load_latest(.., listed_at)`), so the
/// doctor must not select it either.
#[test]
fn gather_ignores_a_future_only_fit() {
    use super::super::doctor::Status;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let now = Utc::now();
    fit_file(root, now + chrono::Duration::hours(1));

    let facts = gather(root, "host-a", now);
    assert_eq!(facts.fit.latest, None);
    assert!(!facts.serving.fit_loaded);
    assert_ne!(status_of(&facts, "serving", "twin_otter_model"), Status::Ok);
    assert_ne!(status_of(&facts, "fit", "coefficient_file"), Status::Ok);
}

/// Regression (#10407 review): beside a newer future-dated fit, the doctor
/// selects the old eligible one, so `fit.coefficient_file` reports its real
/// age instead of hiding it behind the future file.
#[test]
fn gather_selects_the_old_eligible_fit_over_a_future_one() {
    use super::super::doctor::Status;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let now = Utc::now();
    let old_as_of = now - chrono::Duration::days(5);
    let old_id = fit_file(root, old_as_of);
    let future_id = fit_file(root, now + chrono::Duration::hours(1));
    assert_ne!(old_id, future_id);

    let facts = gather(root, "host-a", now);
    assert_eq!(facts.fit.latest, Some((old_id.clone(), old_as_of)));
    assert!(facts.serving.fit_loaded);
    assert_eq!(status_of(&facts, "serving", "twin_otter_model"), Status::Ok);
    assert_eq!(
        status_of(&facts, "fit", "coefficient_file"),
        Status::Fail,
        "a 5-day-old fit is stale; the future file must not mask it"
    );
}

#[test]
fn a_non_github_host_resolves_no_reader() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(reader_in(tmp.path(), "acme/alpha", Some("gitea.example.com")).is_none());
}

/// #10520: the gap-fill count comes from the persisted refresh state, read
/// only; a repo whose state carries no note reports none.
#[test]
fn gather_reads_the_persisted_gap_fill_count() {
    use crate::eta::fleet_signoz_history::{HistoryNote, HistorySource};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let now = Utc::now();
    snapshot(root, "acme/alpha", now);
    snapshot(root, "acme/beta", now);
    let note = HistoryNote {
        at: now,
        source: HistorySource::Signoz,
        gap_fill_calls: 0,
    };
    let mut state = fleet_refresh::RefreshState::new("acme/alpha");
    state.history = Some(note);
    fleet_refresh::write_state(&fleet_refresh::state_path(root, "acme/alpha"), &state).unwrap();

    let facts = gather(root, "host-a", now);
    let by_repo: BTreeMap<&str, Option<HistoryNote>> = facts
        .data
        .repos
        .iter()
        .map(|r| (r.repo.as_str(), r.history))
        .collect();
    assert_eq!(by_repo.get("acme/alpha"), Some(&Some(note)));
    assert_eq!(by_repo.get("acme/beta"), Some(&None));
}
