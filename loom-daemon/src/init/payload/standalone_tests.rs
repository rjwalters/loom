//! Tests for `loom-daemon resync-payload` (#8961), over temp workspaces and a
//! synthetic payload. The fixtures are `payload_tests`'s, so a workspace here
//! is installed exactly as one there is.

use std::fs;

use tempfile::TempDir;

use super::super::{Payload, ResyncRefusal};
use super::{run_with, Changes, Verdict, EXIT_DRIFT, EXIT_FAILED, EXIT_OK};
use crate::init::payload::surfaces::INSTALL_TIME_ONLY;
use crate::init::payload_tests::{
    fake_defaults, freeze, installed_workspace, meta_json, set_exec, snapshot, stamp, touched, v,
    write,
};
use crate::install_compat::INSTALL_METADATA_PATH as META;

/// A payload one file newer and one file larger than the workspace has.
fn drifted(tmp: &TempDir, installed: &str, payload: &str) -> (std::path::PathBuf, Payload) {
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, installed);
    write(&defaults.join("scripts/a.sh"), "#!/bin/sh\necho a2\n");
    set_exec(&defaults.join("scripts/a.sh"));
    write(&defaults.join("docs/new.md"), "new\n");
    (ws, Payload::from_defaults(defaults, stamp(payload)))
}

fn drift() -> Changes {
    Changes {
        added: vec![".loom/docs/new.md".to_string()],
        changed: vec![".loom/scripts/a.sh".to_string()],
        removed: Vec::new(),
    }
}

#[test]
fn dry_run_names_the_changes_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = drifted(&tmp, "0.19.880", "0.19.881");

    let before = freeze(&ws);
    let report = run_with(&payload, &ws, true).unwrap();
    assert_eq!(report.verdict, Verdict::WouldChange(drift()));
    assert_eq!(report.exit_code(), EXIT_DRIFT);
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());

    let text = report.render(&ws);
    assert!(text.contains("  would add .loom/docs/new.md\n"), "{text}");
    assert!(text.contains("  would update .loom/scripts/a.sh\n"), "{text}");
    assert!(text.contains("DRY RUN: 2 file(s) would change"), "{text}");
    assert!(text.contains("Nothing was written"), "{text}");
}

#[test]
fn apply_writes_the_payload_and_stamps_the_install() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = drifted(&tmp, "0.19.880", "0.19.881");

    let before = freeze(&ws);
    let report = run_with(&payload, &ws, false).unwrap();
    assert_eq!(report.verdict, Verdict::Applied(drift()));
    assert_eq!(report.exit_code(), EXIT_OK);
    assert_eq!(
        touched(&ws, &before, &snapshot(&ws)),
        vec![
            ".loom/docs/new.md".to_string(),
            META.to_string(),
            ".loom/scripts/a.sh".to_string()
        ]
    );
    assert_eq!(
        fs::read_to_string(ws.join(".loom/scripts/a.sh")).unwrap(),
        "#!/bin/sh\necho a2\n"
    );
    assert_eq!(fs::read_to_string(ws.join(".loom/docs/new.md")).unwrap(), "new\n");

    // The daemon's version, a full commit and the contract floor; never the
    // `unknown` a source tree without package.json or git would give (#9613).
    let meta = meta_json(&ws);
    assert_eq!(meta["loom_version"], "0.19.881");
    let commit = meta["loom_commit"].as_str().unwrap();
    assert!(commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()), "{commit}");
    assert_eq!(meta["requires_daemon"], "0.19.772");
    assert!(!fs::read_to_string(ws.join(META))
        .unwrap()
        .contains("unknown"));

    let text = report.render(&ws);
    assert!(text.contains("  add .loom/docs/new.md\n"), "{text}");
    assert!(text.contains("  update .loom/scripts/a.sh\n"), "{text}");
    assert!(text.contains("2 file(s) written; install stamped 0.19.881"), "{text}");
}

#[test]
fn a_second_run_writes_nothing_at_all() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = drifted(&tmp, "0.19.880", "0.19.881");
    run_with(&payload, &ws, false).unwrap();

    for dry_run in [true, false] {
        let before = freeze(&ws);
        let report = run_with(&payload, &ws, dry_run).unwrap();
        assert_eq!(report.verdict, Verdict::InSync, "dry_run={dry_run}");
        assert_eq!(report.exit_code(), EXIT_OK);
        assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());
        assert!(report.render(&ws).contains("already in sync with 0.19.881"));
    }
}

/// Never a downgrade: both "repo ahead of daemon" shapes are refused on this
/// path as on the fleet-sync one, with or without `--dry-run`.
#[test]
fn a_repo_ahead_of_the_daemon_is_refused_and_nothing_is_written() {
    for (installed, requires, expected) in [
        (
            "0.19.900",
            None,
            ResyncRefusal::RepoAheadOfDaemon {
                installed: v("0.19.900"),
                running: v("0.19.881"),
            },
        ),
        (
            "0.19.880",
            Some("0.19.950"),
            ResyncRefusal::NeedsNewerDaemon {
                requires: v("0.19.950"),
                running: v("0.19.881"),
            },
        ),
    ] {
        for dry_run in [true, false] {
            let tmp = TempDir::new().unwrap();
            let (ws, payload) = drifted(&tmp, installed, "0.19.881");
            if let Some(req) = requires {
                let mut meta = meta_json(&ws);
                meta["requires_daemon"] = req.into();
                write(&ws.join(META), &serde_json::to_string_pretty(&meta).unwrap());
            }

            let before = freeze(&ws);
            let report = run_with(&payload, &ws, dry_run).unwrap();
            assert_eq!(report.verdict, Verdict::Refused(expected.clone()));
            assert_eq!(report.exit_code(), EXIT_FAILED);
            assert!(report.refused());
            assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());

            let text = report.render(&ws);
            assert!(text.contains("refused, nothing written: repo ahead of daemon"), "{text}");
        }
    }
}

/// A developer's build embeds whatever tree it was built in, so its payload
/// is never applied, and the report says why this binary does not qualify.
#[test]
fn a_build_that_is_not_a_release_is_refused_with_the_reason() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = installed_workspace(tmp.path(), &defaults, "0.19.880");
    write(&defaults.join("docs/new.md"), "new\n");
    let mut dev = stamp("0.19.881");
    dev.release_build = false;
    let payload = Payload::from_defaults(defaults, dev);

    let before = freeze(&ws);
    let mut report = run_with(&payload, &ws, false).unwrap();
    assert_eq!(report.verdict, Verdict::Refused(ResyncRefusal::NotAReleaseBuild));
    assert_eq!(report.exit_code(), EXIT_FAILED);
    assert_eq!(touched(&ws, &before, &snapshot(&ws)), Vec::<String>::new());

    report.provenance =
        Some("not a release build: it was not built by the release workflow".into());
    let text = report.render(&ws);
    assert!(text.contains("not a verified official release build"), "{text}");
    assert!(text.contains("it was not built by the release workflow"), "{text}");
}

#[test]
fn a_workspace_without_loom_is_refused() {
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    let ws = tmp.path().join("bare");
    fs::create_dir_all(ws.join(".git")).unwrap();
    let payload = Payload::from_defaults(defaults, stamp("0.19.881"));

    let report = run_with(&payload, &ws, false).unwrap();
    assert_eq!(report.verdict, Verdict::Refused(ResyncRefusal::NotInstalled));
    assert!(!ws.join(".loom").exists());
}

/// A pin and a symlinked target are the workspace's to keep; the rest of the
/// payload still lands.
#[cfg(unix)]
#[test]
fn pins_and_symlinked_targets_are_left_alone() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = drifted(&tmp, "0.19.880", "0.19.881");
    write(&ws.join(".loom/resync-ignore"), "scripts/a.sh\n");
    write(&ws.join(".loom/scripts/a.sh"), "#!/bin/sh\nlocal fix\n");
    let outside = tmp.path().join("elsewhere.md");
    write(&outside, "outside\n");
    fs::remove_file(ws.join(".loom/roles/builder.md")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join(".loom/roles/builder.md")).unwrap();
    write(&payload.defaults().join("roles/builder.md"), "builder v2\n");

    let report = run_with(&payload, &ws, false).unwrap();
    assert_eq!(
        report.verdict,
        Verdict::Applied(Changes {
            added: vec![".loom/docs/new.md".to_string()],
            ..Changes::default()
        })
    );
    assert_eq!(
        fs::read_to_string(ws.join(".loom/scripts/a.sh")).unwrap(),
        "#!/bin/sh\nlocal fix\n"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside\n");
    assert!(fs::symlink_metadata(ws.join(".loom/roles/builder.md"))
        .unwrap()
        .file_type()
        .is_symlink());
}

/// The script-only steps this path skips are named, from the one table.
#[test]
fn the_report_names_what_this_path_never_refreshes() {
    let tmp = TempDir::new().unwrap();
    let (ws, payload) = drifted(&tmp, "0.19.880", "0.19.881");
    for dry_run in [true, false] {
        let text = run_with(&payload, &ws, dry_run).unwrap().render(&ws);
        for (path, _) in INSTALL_TIME_ONLY {
            assert!(text.contains(path), "{path} missing from: {text}");
        }
    }
}
