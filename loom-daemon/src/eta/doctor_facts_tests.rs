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

#[test]
fn a_non_github_host_resolves_no_reader() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(reader_in(tmp.path(), "acme/alpha", Some("gitea.example.com")).is_none());
}
