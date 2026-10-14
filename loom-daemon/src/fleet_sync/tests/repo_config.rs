//! The per-repo fleet tier half of a fleet-sync pass (#11738).

use std::collections::BTreeMap;
use std::path::Path;

use chrono::TimeZone;
use serde_json::json;

use super::*;
use crate::config_resolver::FLEET_REPO_CONFIG_REL;
use crate::fleet_store::test_support::snapshot_of;
use crate::fleet_store::FLEET_JSON_PATH;
use crate::fleet_sync::{render_line, FleetSyncStatus};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
}

/// A compiled store rooted at `root`: `app` (desired, with config), `bare`
/// (desired, no config), `vault` (firewall, with config) and `side` (not
/// managed, with config).
fn snapshot(root: &Path, app_config: Option<serde_json::Value>) -> Snapshot {
    let mut app = json!({"name": "app", "dir": "app", "fleet": true, "weight": 1});
    if let Some(c) = app_config {
        app["config"] = c;
    }
    let doc = json!({
        "_generated": {"schema_version": 1},
        "root": root.display().to_string(),
        "repos": [
            app,
            {"name": "bare", "dir": "bare", "fleet": true, "weight": 1},
            {"name": "vault", "dir": "vault", "fleet": false, "firewall": true, "config": {"a": 1}},
            {"name": "side", "dir": "side", "fleet": false, "config": {"a": 1}}
        ],
        "state": {},
        "config": {"defaults": {}}
    });
    let mut files = BTreeMap::new();
    files.insert(FLEET_JSON_PATH.to_string(), doc.to_string());
    snapshot_of(&files)
}

/// Every repo in [`snapshot`] cloned under a fresh root.
fn cloned_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for r in ["app", "bare", "vault", "side"] {
        std::fs::create_dir_all(dir.path().join(r).join(".git")).unwrap();
    }
    dir
}

fn is_cloned(p: &Path) -> bool {
    p.join(".git").exists()
}

fn file(root: &Path, repo: &str) -> std::path::PathBuf {
    root.join(repo).join(FLEET_REPO_CONFIG_REL)
}

#[test]
#[serial_test::serial(loom_config_env)]
fn write_mode_materializes_the_record_config_and_is_then_idempotent() {
    let root = cloned_root();
    let snap = snapshot(root.path(), Some(json!({"autonomous": {"onIdle": []}, "guards": {}})));
    let got = pass(&snap, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    assert_eq!(got.error, None);
    assert_eq!(got.repos.len(), 1, "only `app` has config or a file: {got:?}");
    let r = &got.repos[0];
    assert_eq!((r.name.as_str(), r.wrote, r.present, r.drifted), ("app", true, true, true));
    assert_eq!(r.keys, vec!["autonomous".to_string(), "guards".to_string()]);
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file(root.path(), "app")).unwrap()).unwrap();
    assert_eq!(on_disk, json!({"autonomous": {"onIdle": []}, "guards": {}}));
    // The resolver now sees it, above the committed config.
    std::fs::create_dir_all(root.path().join("app/.loom")).unwrap();
    std::fs::write(
        root.path().join("app/.loom/config.json"),
        r#"{"autonomous": {"onIdle": ["auditor"]}}"#,
    )
    .unwrap();
    // Under `cfg(test)` the machine tier is off unless the env names a file.
    let effective = crate::config_resolver::resolve_effective_config(&root.path().join("app"));
    assert_eq!(effective["autonomous"]["onIdle"], json!([]));

    let again = pass(&snap, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    let r = &again.repos[0];
    assert_eq!((r.wrote, r.drifted, r.present), (false, false, true), "no-op re-render");
    assert!(!again.drifted());
    let lines = lines(&again).join("\n");
    assert!(lines.contains("per-repo fleet tier: app applied"), "{lines}");
    assert!(lines.contains("2 keys: autonomous, guards"), "{lines}");
}

#[test]
fn never_writes_under_a_firewall_unmanaged_or_uncloned_repo() {
    let root = cloned_root();
    std::fs::remove_dir_all(root.path().join("bare/.git")).unwrap();
    let snap = snapshot(root.path(), Some(json!({"a": 1})));
    pass(&snap, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    for repo in ["vault", "side", "bare"] {
        assert!(!file(root.path(), repo).exists(), "{repo} must not get a per-repo tier");
    }
    assert!(file(root.path(), "app").exists());
}

#[test]
fn check_mode_reports_drift_and_writes_nothing() {
    let root = cloned_root();
    let snap = snapshot(root.path(), Some(json!({"a": 1})));
    let got = pass(&snap, Path::new("/home/x"), &is_cloned, Mode::Check, now());
    let r = &got.repos[0];
    assert_eq!((r.drifted, r.wrote, r.present), (true, false, false));
    assert_eq!(r.detail.as_deref(), Some("does not exist"));
    assert!(got.drifted());
    assert!(!file(root.path(), "app").exists(), "a check pass writes nothing");
    let lines = lines(&got).join("\n");
    assert!(lines.contains("app DRIFT"), "{lines}");

    // Nor does it remove a file the store no longer wants.
    let stale = file(root.path(), "bare");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, r#"{"old": true}"#).unwrap();
    let got = pass(&snap, Path::new("/home/x"), &is_cloned, Mode::Check, now());
    assert!(stale.exists());
    assert!(got
        .repos
        .iter()
        .any(|r| r.name == "bare" && r.drifted && r.present));
}

#[test]
fn taking_the_config_out_of_the_store_removes_the_file_with_a_backup() {
    let root = cloned_root();
    let with = snapshot(root.path(), Some(json!({"a": 1})));
    pass(&with, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    assert!(file(root.path(), "app").exists());

    let without = snapshot(root.path(), None);
    let got = pass(&without, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    let r = &got.repos[0];
    assert_eq!((r.removed, r.present, r.drifted), (true, false, true));
    assert!(r.keys.is_empty());
    assert!(!file(root.path(), "app").exists());
    let backup = root
        .path()
        .join("app/.loom-local/fleet-repo.json.fleet-store-bak-20261013T120000Z");
    assert_eq!(std::fs::read_to_string(backup).unwrap().trim(), "{\n  \"a\": 1\n}");
    assert!(lines(&got).join("\n").contains("app removed"));

    // Once removed, there is nothing left to say about it.
    let quiet = pass(&without, Path::new("/home/x"), &is_cloned, Mode::Write, now());
    assert!(quiet.is_unset(), "{quiet:?}");
}

#[test]
fn a_rewrite_keeps_exactly_one_backup() {
    let root = cloned_root();
    pass(
        &snapshot(root.path(), Some(json!({"a": 1}))),
        Path::new("/x"),
        &is_cloned,
        Mode::Write,
        now(),
    );
    let later = now() + chrono::Duration::seconds(60);
    pass(
        &snapshot(root.path(), Some(json!({"a": 2}))),
        Path::new("/x"),
        &is_cloned,
        Mode::Write,
        now(),
    );
    pass(
        &snapshot(root.path(), Some(json!({"a": 3}))),
        Path::new("/x"),
        &is_cloned,
        Mode::Write,
        later,
    );
    let backups: Vec<_> = std::fs::read_dir(root.path().join("app/.loom-local"))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains(".fleet-store-bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
}

#[test]
fn a_render_error_is_reported_not_raised() {
    let got = apply(Err(anyhow::anyhow!("boom")), Mode::Write, now(), &None);
    assert_eq!(got.error.as_deref(), Some("boom"));
    assert!(lines(&got)[0].contains("per-repo fleet tier: ERROR — boom"));
}

#[test]
fn an_unset_half_is_omitted_from_the_snapshot_json() {
    let mut status = crate::fleet_sync::tests::sample_status();
    let json = serde_json::to_value(&status).unwrap();
    assert!(json.get("repoConfig").is_none(), "pre-#11738 snapshot shape is unchanged");
    status.repo_config.repos.push(RepoTierReport {
        name: "app".into(),
        path: "/r/app/.loom-local/fleet-repo.json".into(),
        keys: vec!["autonomous".into()],
        present: true,
        drifted: false,
        wrote: false,
        removed: false,
        detail: None,
    });
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["repoConfig"]["repos"][0]["keys"], json!(["autonomous"]));
    assert_eq!(json["repoConfig"]["repos"][0]["present"], json!(true));
    let back: FleetSyncStatus = serde_json::from_value(json).unwrap();
    assert_eq!(back, status);
    let line = render_line(Some(&status), status.at).unwrap();
    assert!(line.contains("per-repo fleet tier: app applied"), "{line}");
}
