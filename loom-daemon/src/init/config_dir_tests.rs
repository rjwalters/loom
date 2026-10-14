//! Tests for the `defaults/config/` install step (#9129).
//!
//! `scripts/install/manifest.sh` records `defaults/config/X` as the installed
//! file `.loom/config/X`, but only the shell installers copied it, so a
//! standalone `loom-daemon init` left `.loom/config/skill-routes.json` absent
//! and the skill router silently off. These tests cover the copy
//! ([`install_config_dir_files`]), its consumer-configuration rules, and the
//! reason it is not part of the payload step: a daemon resync must never
//! create, overwrite or remove these files.
//!
//! A sibling of `tests.rs` for the same reason as `loom_payload_tree_tests.rs`:
//! that file is frozen by `scripts/check-file-size-budget.sh`.

use super::loom_payload_tree_tests::{installed_files_manifest, repo_root_for_tests};
use super::payload::{apply, materialize_with, Payload, ResyncOutcome};
use super::payload_tests::{fake_defaults, stamp, write};
use super::*;
use tempfile::TempDir;

const ROUTES: &str = ".loom/config/skill-routes.json";
const SHIPPED: &str = "{\"routes\": []}\n";
const EDITED: &str = "{\"routes\": [\"mine\"]}\n";

/// A git workspace beside a fixture `defaults/` whose `config/` holds `files`.
fn fixture(files: &[(&str, &str)]) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("ws");
    let defaults = tmp.path().join("defaults");
    fs::create_dir_all(workspace.join(".git")).unwrap();
    write(&defaults.join("config.json"), "{}");
    for (name, contents) in files {
        write(&defaults.join("config").join(name), contents);
    }
    (tmp, workspace, defaults)
}

fn init(workspace: &Path, defaults: &Path, force: bool) -> InitReport {
    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), force)
        .expect("init failed")
}

#[test]
fn test_real_defaults_skill_routes_is_installed_byte_identical() {
    let defaults = repo_root_for_tests().join("defaults");
    let shipped = defaults.join("config/skill-routes.json");
    assert!(shipped.is_file(), "defaults/config/skill-routes.json is gone: fixture lost");

    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let report = init(tmp.path(), &defaults, false);

    assert_eq!(
        fs::read(tmp.path().join(ROUTES)).expect("skill-routes.json was not installed (#9129)"),
        fs::read(&shipped).unwrap()
    );
    assert!(report.added.iter().any(|p| p == ROUTES), "{:?}", report.added);
}

#[test]
fn test_standalone_init_materializes_the_whole_installed_files_manifest() {
    // The whole-manifest form of
    // `test_defaults_loom_tree_copy_and_manifest_enumerations_agree`: every
    // path `install-metadata.json` will claim is installed exists after a
    // standalone init of a fresh workspace, with no installer script run.
    let repo_root = repo_root_for_tests();
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    init(tmp.path(), &repo_root.join("defaults"), false);

    let manifest = installed_files_manifest(&repo_root, tmp.path());
    assert!(manifest.len() > 100, "the real manifest lists hundreds of files");
    assert!(manifest.contains(ROUTES), "{ROUTES} left the manifest: test lost its subject");

    let missing: Vec<&String> = manifest
        .iter()
        .filter(|p| fs::symlink_metadata(tmp.path().join(p)).is_err())
        .collect();
    assert!(
        missing.is_empty(),
        "recorded in installed_files but never written by loom-daemon init: {missing:?}"
    );
}

#[test]
fn test_reinstall_preserves_an_edited_config_file() {
    let (_tmp, workspace, defaults) = fixture(&[("skill-routes.json", SHIPPED)]);
    let first = init(&workspace, &defaults, false);
    assert!(first.added.iter().any(|p| p == ROUTES), "{:?}", first.added);
    assert_eq!(fs::read_to_string(workspace.join(ROUTES)).unwrap(), SHIPPED);

    fs::write(workspace.join(ROUTES), EDITED).unwrap();
    let second = init(&workspace, &defaults, false);

    assert_eq!(fs::read_to_string(workspace.join(ROUTES)).unwrap(), EDITED);
    assert!(second.preserved.iter().any(|p| p == ROUTES), "{:?}", second.preserved);
    assert!(!second.updated.iter().any(|p| p == ROUTES), "{:?}", second.updated);
    assert!(
        !second
            .verification_failures
            .iter()
            .any(|f| f.contains(ROUTES)),
        "{:?}",
        second.verification_failures
    );
}

#[test]
fn test_force_preserves_an_edited_config_file() {
    // Init never overwrites these files, with or without --force:
    // `install.sh --quick --confirm-reinstall` passes --force unconditionally,
    // and a legacy uninstall leaves `.loom/config/` in place before it.
    let (_tmp, workspace, defaults) = fixture(&[("skill-routes.json", SHIPPED)]);
    init(&workspace, &defaults, false);
    fs::write(workspace.join(ROUTES), EDITED).unwrap();

    let report = init(&workspace, &defaults, true);

    assert_eq!(fs::read_to_string(workspace.join(ROUTES)).unwrap(), EDITED);
    assert!(report.preserved.iter().any(|p| p == ROUTES), "{:?}", report.preserved);
    assert!(!report.updated.iter().any(|p| p == ROUTES), "{:?}", report.updated);
}

#[test]
fn test_force_installs_a_missing_config_file() {
    // The manifest-era `--confirm-reinstall` shape: the uninstall removed the
    // file, then init runs with --force and copies the shipped one.
    let (_tmp, workspace, defaults) = fixture(&[("skill-routes.json", SHIPPED)]);
    init(&workspace, &defaults, false);
    fs::remove_file(workspace.join(ROUTES)).unwrap();

    let report = init(&workspace, &defaults, true);

    assert_eq!(fs::read_to_string(workspace.join(ROUTES)).unwrap(), SHIPPED);
    assert!(report.added.iter().any(|p| p == ROUTES), "{:?}", report.added);
    assert!(!report.updated.iter().any(|p| p == ROUTES), "{:?}", report.updated);
}

#[test]
fn test_every_json_file_in_defaults_config_is_copied() {
    // The step walks the directory; it does not name skill-routes.json.
    let (_tmp, workspace, defaults) = fixture(&[
        ("skill-routes.json", SHIPPED),
        ("second.json", "{\"b\": 2}\n"),
        ("notes.txt", "not json\n"),
    ]);
    write(&defaults.join("config/nested/deep.json"), "{}\n");

    let report = init(&workspace, &defaults, false);

    for (name, contents) in [
        ("skill-routes.json", SHIPPED),
        ("second.json", "{\"b\": 2}\n"),
    ] {
        assert_eq!(
            fs::read_to_string(workspace.join(".loom/config").join(name)).unwrap(),
            contents
        );
        assert!(report.added.contains(&format!(".loom/config/{name}")), "{:?}", report.added);
    }
    // Same glob as the shell copies: top-level `*.json` only.
    assert!(!workspace.join(".loom/config/notes.txt").exists());
    assert!(!workspace.join(".loom/config/nested").exists());
}

#[test]
fn test_reinstall_restores_only_the_missing_config_file() {
    let (_tmp, workspace, defaults) = fixture(&[
        ("skill-routes.json", SHIPPED),
        ("second.json", "{\"b\": 2}\n"),
    ]);
    init(&workspace, &defaults, false);
    fs::write(workspace.join(ROUTES), EDITED).unwrap();
    fs::remove_file(workspace.join(".loom/config/second.json")).unwrap();

    let report = init(&workspace, &defaults, false);

    assert_eq!(fs::read_to_string(workspace.join(ROUTES)).unwrap(), EDITED);
    assert!(workspace.join(".loom/config/second.json").is_file());
    assert!(
        report.added.iter().any(|p| p == ".loom/config/second.json"),
        "{:?}",
        report.added
    );
}

#[test]
fn test_init_succeeds_without_a_defaults_config_dir() {
    let (_tmp, workspace, defaults) = fixture(&[]);
    assert!(!defaults.join("config").exists());

    let report = init(&workspace, &defaults, false);

    assert!(!workspace.join(".loom/config").exists(), "no config/ shipped, none created");
    assert!(
        !report.added.iter().any(|p| p.starts_with(".loom/config/")),
        "{:?}",
        report.added
    );
}

#[test]
fn test_resync_never_creates_overwrites_or_removes_a_config_file() {
    // Why the step is not in `install_payload_files`: that is what a daemon
    // resync runs. The metadata lists the file as installed, the way a real
    // install's does, so an ownership-gated removal would see it as Loom's.
    let tmp = TempDir::new().unwrap();
    let defaults = fake_defaults(tmp.path());
    write(&defaults.join("config/skill-routes.json"), SHIPPED);
    let payload = Payload::from_defaults(defaults, stamp("0.19.900"));
    let ws = tmp.path().join("ws");
    fs::create_dir_all(ws.join(".git")).unwrap();
    write(
        &ws.join(".loom/install-metadata.json"),
        &format!("{{\"loom_version\": \"0.19.0\", \"installed_files\": [\"{ROUTES}\"]}}\n"),
    );
    let touches_config = |ws: &Path| {
        let diff = materialize_with(&payload, ws).unwrap();
        let hit = [&diff.added, &diff.changed, &diff.removed]
            .into_iter()
            .flatten()
            .any(|p| p.starts_with(".loom/config/"));
        (diff, hit)
    };

    // Deleted (the documented way to switch the router off): stays deleted.
    let (diff, hit) = touches_config(&ws);
    assert!(!hit, "a resync must not report .loom/config/: {diff:?}");
    assert!(!diff.is_empty(), "the fixture resync has work to do");
    assert!(matches!(apply(&ws, &diff).unwrap(), ResyncOutcome::Applied { .. }));
    assert!(!ws.join(ROUTES).exists(), "a resync re-created a deleted skill-routes.json");

    // Edited: neither overwritten nor removed, on a resync with work to do.
    write(&ws.join(ROUTES), EDITED);
    fs::remove_file(ws.join(".loom/docs/d.md")).unwrap();
    let (diff, hit) = touches_config(&ws);
    assert!(!hit, "a resync must not report .loom/config/: {diff:?}");
    assert!(matches!(apply(&ws, &diff).unwrap(), ResyncOutcome::Applied { .. }));
    assert_eq!(fs::read_to_string(ws.join(ROUTES)).unwrap(), EDITED);
}
