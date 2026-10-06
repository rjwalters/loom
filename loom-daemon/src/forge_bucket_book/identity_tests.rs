#![allow(clippy::unwrap_used, clippy::expect_used)]
//! #10571: a credential directory's readings are keyed by the identity
//! minted into it, so one key is one GitHub bucket.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use super::super::{
    classify_dir, load, persist, probe_all_with, snapshot, BucketKey, Reading, Resource, Source,
    NO_INSTALLATION,
};
use super::*;
use crate::forge_identity::sidecar::write_sidecar;
use crate::forge_identity::{Sidecar, SidecarRole};

/// A git workspace whose `origin` is `github.com/<owner>/ws`, with a roster
/// writer App `roster_app` when given.
fn workspace(owner: &str, roster_app: Option<&str>) -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    let url = format!("https://github.com/{owner}/ws");
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", &url]] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(ws.path())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }
    if let Some(app) = roster_app {
        std::fs::create_dir_all(ws.path().join(".loom")).unwrap();
        std::fs::write(
            ws.path().join(".loom/config.json"),
            format!(
                r#"{{"forge": {{"githubApp": {{"appId": "{app}", "privateKeyPath": "/k.pem"}}}}}}"#
            ),
        )
        .unwrap();
    }
    ws
}

/// A published writer directory: a token file and a writer sidecar.
fn writer_dir(dir: &Path, app: &str, installation: &str, owner: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("hosts.yml"), "").unwrap();
    let side = Sidecar {
        app_id: app.into(),
        installation_id: installation.into(),
        owner: Some(owner.into()),
        role: SidecarRole::Writer,
        expires_at: "2099-01-01T00:00:00Z".into(),
        ..Sidecar::default()
    };
    write_sidecar(dir, &side).unwrap();
}

/// A stub `gh` whose `rate_limit` answer's `core.used` is 111 under the
/// primary directory and 222 under any other.
fn stub_gh(bin: &Path) -> std::path::PathBuf {
    let reset = chrono::Utc::now().timestamp() + 1800;
    let gh = bin.join("gh-probe");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \"$GH_CONFIG_DIR\" in */gh-config) used=111;; *) used=222;; esac\n\
             printf 'HTTP/2.0 200 OK\\r\\n\\r\\n{{\"resources\":{{\"core\":{{\"limit\":5000,\"used\":%s,\"remaining\":1,\"reset\":{reset}}}}}}}' \"$used\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

#[test]
fn two_writer_dirs_with_different_sidecar_apps_under_one_owner_are_two_buckets() {
    let owner = "acme10571a";
    let ws = workspace(owner, Some("1057100"));
    let primary = ws.path().join(".loom/gh-config");
    let by_owner = ws.path().join(".loom/gh-config-by-owner").join(owner);
    writer_dir(&primary, "1057101", "7", owner);
    writer_dir(&by_owner, "1057102", "9", owner);
    let bin = tempfile::tempdir().unwrap();
    let gh = stub_gh(bin.path());

    assert_eq!(probe_all_with(ws.path(), Some(&gh), SystemTime::now()), 2);
    let now = chrono::Utc::now().timestamp();
    let ours: Vec<(String, String, String, Option<u64>)> = snapshot(now)
        .into_iter()
        .filter(|(k, _)| k.owner == owner)
        .map(|(k, r)| (k.account, k.installation, k.resource.as_str().to_string(), r.used))
        .collect();
    assert_eq!(
        ours,
        [
            ("app-1057101".into(), "7".into(), "core".into(), Some(111)),
            ("app-1057102".into(), "9".into(), "core".into(), Some(222)),
        ],
        "one key per minted App, neither under the roster's app-1057100"
    );
}

#[test]
fn two_minters_republishing_one_dir_never_share_a_key() {
    let owner = "acme10571b";
    let ws = workspace(owner, None);
    let primary = ws.path().join(".loom/gh-config");
    let bin = tempfile::tempdir().unwrap();
    let gh = stub_gh(bin.path());
    let mut seen = Vec::new();
    // Two Apps take turns minting into the same writer directory.
    for (app, installation) in [("1057111", "71"), ("1057112", "72"), ("1057111", "71")] {
        writer_dir(&primary, app, installation, owner);
        // A fresh mtime/length stamp each turn, as a real republish has.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let id = dir_identity(&primary, &classify_dir(&primary)).unwrap();
        assert_eq!(id.source, IdentitySource::Sidecar);
        seen.push((id.account, id.installation));
        probe_all_with(ws.path(), Some(&gh), SystemTime::now());
    }
    assert_eq!(
        seen,
        [
            ("app-1057111".into(), Some("71".into())),
            ("app-1057112".into(), Some("72".into())),
            ("app-1057111".into(), Some("71".into())),
        ]
    );
    let now = chrono::Utc::now().timestamp();
    let keys: Vec<(String, String)> = snapshot(now)
        .into_iter()
        .filter(|(k, _)| k.owner == owner)
        .map(|(k, _)| (k.account, k.installation))
        .collect();
    assert_eq!(
        keys,
        [
            ("app-1057111".into(), "71".into()),
            ("app-1057112".into(), "72".into())
        ]
    );
}

#[test]
fn a_writer_dir_without_a_sidecar_keys_by_roster_and_remote_with_installation_dash() {
    let owner = "acme10571c";
    let ws = workspace(owner, Some("1057121"));
    let primary = ws.path().join(".loom/gh-config");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::write(primary.join("hosts.yml"), "").unwrap();
    let id = dir_identity(&primary, &classify_dir(&primary)).unwrap();
    assert_eq!(
        id,
        CredIdentity {
            account: "app-1057121".into(),
            owner: Some(owner.into()),
            installation: None,
            source: IdentitySource::Derived,
        }
    );
    let bin = tempfile::tempdir().unwrap();
    probe_all_with(ws.path(), Some(&stub_gh(bin.path())), SystemTime::now());
    let now = chrono::Utc::now().timestamp();
    let key = BucketKey::new("app-1057121", owner, Resource::Core);
    let (held, _) = snapshot(now)
        .into_iter()
        .find(|(k, _)| *k == key)
        .expect("booked under the roster and the remote");
    assert_eq!(held.installation, NO_INSTALLATION);
}

#[test]
fn a_sidecar_that_disagrees_with_the_roster_wins_and_bumps_the_mismatch_counter() {
    let owner = "acme10571d";
    let ws = workspace(owner, Some("1057131"));
    let by_owner = ws.path().join(".loom/gh-config-by-owner").join(owner);
    let before = crate::forge_call_stats::counters::get(WRITER_IDENTITY_MISMATCH);
    writer_dir(&by_owner, "1057132", "33", owner);
    let class = classify_dir(&by_owner);
    let id = dir_identity(&by_owner, &class).unwrap();
    assert_eq!(
        (id.account.as_str(), id.installation.as_deref(), id.source),
        ("app-1057132", Some("33"), IdentitySource::Sidecar)
    );
    let _ = dir_identity(&by_owner, &class);
    assert_eq!(
        crate::forge_call_stats::counters::get(WRITER_IDENTITY_MISMATCH),
        before + 1,
        "counted once per directory and minted identity"
    );

    // A sidecar that agrees with the roster is no mismatch.
    let agreeing = ws.path().join(".loom/gh-config");
    writer_dir(&agreeing, "1057131", "34", owner);
    let id = dir_identity(&agreeing, &classify_dir(&agreeing)).unwrap();
    assert_eq!(id.account, "app-1057131");
    assert_eq!(crate::forge_call_stats::counters::get(WRITER_IDENTITY_MISMATCH), before + 1);
}

#[test]
fn installation_witnesses_a_key_without_splitting_it() {
    let reading = |used, at| Reading {
        limit: Some(5000),
        remaining: Some(5000 - used),
        used: Some(used),
        reset_epoch: at + 1800,
        observed_at: at,
        source: Source::Probe,
    };
    let now = chrono::Utc::now().timestamp();
    let witnessed =
        BucketKey::new("app-1057141", "acme10571e", Resource::Core).with_installation(Some("41"));
    let bare = BucketKey::new("app-1057141", "acme10571e", Resource::Core);
    assert_eq!(witnessed, bare, "the installation is not part of the key");
    super::super::insert(witnessed, reading(10, now - 2));
    // A later reading that does not know the installation keeps the witness.
    super::super::insert(bare.clone(), reading(20, now - 1));
    let held: Vec<_> = snapshot(now)
        .into_iter()
        .filter(|(k, _)| *k == bare)
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!((held[0].0.installation.as_str(), held[0].1.used), ("41", Some(20)));
}

#[test]
fn an_old_snapshot_without_installation_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = chrono::Utc::now().timestamp();
    std::fs::write(
        dir.path().join(super::super::SNAPSHOT_FILE),
        format!(
            r#"[{{"key":{{"account":"app-1057151","owner":"acme","resource":"core"}},"reading":{{"limit":5000,"remaining":4880,"used":120,"reset_epoch":{},"observed_at":{},"source":"probe"}}}}]"#,
            now + 1200,
            now - 30
        ),
    )
    .unwrap();
    let loaded = load(dir.path(), now);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0.installation, NO_INSTALLATION);

    // And a new one round-trips the witness.
    super::super::insert(
        BucketKey::new("app-1057152", "acme", Resource::Core).with_installation(Some("52")),
        Reading {
            limit: Some(5000),
            remaining: Some(1),
            used: Some(4999),
            reset_epoch: now + 1200,
            observed_at: now,
            source: Source::Header,
        },
    );
    persist(dir.path(), now).unwrap();
    let back = load(dir.path(), now);
    let (k, _) = back
        .iter()
        .find(|(k, _)| k.account == "app-1057152")
        .unwrap();
    assert_eq!(k.installation, "52");
}
