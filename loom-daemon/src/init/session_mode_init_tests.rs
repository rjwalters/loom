//! End-to-end coverage for `loom-daemon init --mode session` (issue #8884).
//!
//! `session_mode.rs`'s own unit tests cover the key-set transform in isolation.
//! These drive the real `initialize_workspace_with_mode` against a throwaway
//! workspace, because the properties that matter are about what lands on disk
//! across a fresh install and a later reinstall — the `loom update` path — not
//! about the transform.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::{initialize_workspace, initialize_workspace_with_mode, InstallMode};

/// The shipped template's shape, reduced to what these tests assert on: a
/// non-empty `terminals` array and a couple of unrelated keys.
const TEMPLATE: &str = r#"{
  "version": "2",
  "offlineMode": false,
  "terminals": [
    {"id": "terminal-1", "name": "Judge"},
    {"id": "terminal-2", "name": "Curator"}
  ]
}"#;

/// Build a minimal workspace + `defaults/` pair. The defaults dir MUST be named
/// `defaults` so the metadata writer can derive `loom_source` (see tests.rs).
fn fixture(template: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().to_path_buf();
    let defaults = temp.path().join("defaults");
    fs::create_dir(workspace.join(".git")).unwrap();
    fs::create_dir_all(defaults.join("roles")).unwrap();
    fs::write(defaults.join("config.json"), template).unwrap();
    fs::write(defaults.join("roles").join("builder.md"), "builder").unwrap();
    (temp, workspace, defaults)
}

fn init(workspace: &std::path::Path, defaults: &std::path::Path, mode: InstallMode) {
    let result = initialize_workspace_with_mode(
        workspace.to_str().unwrap(),
        defaults.to_str().unwrap(),
        false,
        mode,
    );
    assert!(result.is_ok(), "init failed: {result:?}");
}

fn read_config(workspace: &std::path::Path) -> Value {
    let raw = fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn session_mode_fresh_install_writes_empty_terminals_and_the_marker() {
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    init(&workspace, &defaults, InstallMode::Session);

    let config = read_config(&workspace);
    assert_eq!(config["terminals"], serde_json::json!([]), "AC: terminals must be empty");
    assert_eq!(config["mode"], serde_json::json!("session"), "AC: the marker must be persisted");
    // The scope question #8884 left open, resolved: the daemon tier's two work
    // generators are written off too, not just the tmux pool's terminals.
    assert_eq!(config["autonomous"]["roleRunner"]["enabled"], serde_json::json!(false));
    assert_eq!(config["autonomous"]["workFinder"]["enabled"], serde_json::json!(false));
    // Unrelated template keys still arrive.
    assert_eq!(config["version"], serde_json::json!("2"));
}

#[test]
fn default_install_is_byte_identical_to_the_pre_change_behaviour() {
    // AC: existing (non-session-mode) installs are unaffected. Both the
    // three-argument entry point and an explicit `--mode default` must leave
    // the shipped terminals array alone and write NO mode key at all.
    let (_a, ws_implicit, defaults_a) = fixture(TEMPLATE);
    let result =
        initialize_workspace(ws_implicit.to_str().unwrap(), defaults_a.to_str().unwrap(), false);
    assert!(result.is_ok(), "init failed: {result:?}");

    let (_b, ws_explicit, defaults_b) = fixture(TEMPLATE);
    init(&ws_explicit, &defaults_b, InstallMode::Default);

    for workspace in [&ws_implicit, &ws_explicit] {
        let config = read_config(workspace);
        assert_eq!(config["terminals"].as_array().unwrap().len(), 2);
        assert!(config.get("mode").is_none(), "a default install must write no mode key");
        assert!(
            config.get("autonomous").is_none(),
            "a default install must not write autonomous"
        );
    }
    // …and the two paths agree byte-for-byte.
    assert_eq!(
        fs::read_to_string(ws_implicit.join(".loom").join("config.json")).unwrap(),
        fs::read_to_string(ws_explicit.join(".loom").join("config.json")).unwrap()
    );
}

#[test]
fn a_reinstall_without_the_flag_does_not_restore_the_default_terminals() {
    // AC: the marker survives `loom update` / any later reinstall. This is the
    // regression that matters most — a resync or reinstall that silently
    // restored the four-terminal array would re-arm the tmux pool in a repo
    // installed specifically to never have one.
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    init(&workspace, &defaults, InstallMode::Session);
    let after_install = fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap();

    // Reinstall the way `loom update` does: no --mode, merge mode.
    init(&workspace, &defaults, InstallMode::Default);
    let config = read_config(&workspace);
    assert_eq!(config["terminals"], serde_json::json!([]), "reinstall restored the terminals");
    assert_eq!(config["mode"], serde_json::json!("session"), "reinstall dropped the marker");

    // Byte-idempotent, so the reinstall leaves config.json clean in git.
    assert_eq!(
        after_install,
        fs::read_to_string(workspace.join(".loom").join("config.json")).unwrap()
    );

    // …and a --force reinstall behaves the same (force does not bypass the
    // merge-aware config path).
    let forced = initialize_workspace_with_mode(
        workspace.to_str().unwrap(),
        defaults.to_str().unwrap(),
        true,
        InstallMode::Default,
    );
    assert!(forced.is_ok(), "forced reinstall failed: {forced:?}");
    assert_eq!(read_config(&workspace)["terminals"], serde_json::json!([]));
}

#[test]
fn session_mode_is_re_asserted_over_a_hand_reverted_config() {
    // The marker is the source of truth, so an init that finds it re-applies the
    // whole key set: a hand-pasted terminals array or a re-enabled work finder
    // is corrected, not preserved. Leaving session mode means removing the
    // marker (documented in defaults/docs/session-mode.md), not fighting it.
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    init(&workspace, &defaults, InstallMode::Session);

    let dst = workspace.join(".loom").join("config.json");
    let mut config = read_config(&workspace);
    config["terminals"] = serde_json::json!([{"id": "terminal-1"}]);
    config["autonomous"]["workFinder"]["enabled"] = serde_json::json!(true);
    fs::write(&dst, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    init(&workspace, &defaults, InstallMode::Default);
    let config = read_config(&workspace);
    assert_eq!(config["terminals"], serde_json::json!([]));
    assert_eq!(config["autonomous"]["workFinder"]["enabled"], serde_json::json!(false));
}

#[test]
fn removing_the_marker_leaves_session_mode() {
    // The documented exit: drop the `mode` key, and the next init stops
    // asserting the key set (the consumer's own terminals then win the merge).
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    init(&workspace, &defaults, InstallMode::Session);

    let dst = workspace.join(".loom").join("config.json");
    let mut config = read_config(&workspace);
    config.as_object_mut().unwrap().remove("mode");
    config["terminals"] = serde_json::json!([{"id": "terminal-1"}]);
    fs::write(&dst, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    init(&workspace, &defaults, InstallMode::Default);
    let config = read_config(&workspace);
    assert_eq!(config["terminals"].as_array().unwrap().len(), 1);
    assert!(config.get("mode").is_none());
}

#[test]
fn session_mode_preserves_consumer_overrides_in_the_merge() {
    // Session mode rides on top of the existing existing-values-win merge, so a
    // documented consumer override such as worktree.root still survives.
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(
        workspace.join(".loom").join("config.json"),
        r#"{"worktree": {"root": "/custom/root"}, "terminals": [{"id": "terminal-9"}]}"#,
    )
    .unwrap();

    init(&workspace, &defaults, InstallMode::Session);
    let config = read_config(&workspace);
    assert_eq!(config["worktree"]["root"], serde_json::json!("/custom/root"));
    assert_eq!(
        config["terminals"],
        serde_json::json!([]),
        "the flag wins over the existing array"
    );
    assert_eq!(config["mode"], serde_json::json!("session"));
}

#[test]
fn session_mode_refuses_an_unparseable_template_instead_of_failing_open() {
    // A safety flag that silently installs an unattended-capable config is worse
    // than an aborted install. The same template WITHOUT the flag still degrades
    // to the historical verbatim copy.
    let (_temp, workspace, defaults) = fixture("{not json");
    let result = initialize_workspace_with_mode(
        workspace.to_str().unwrap(),
        defaults.to_str().unwrap(),
        false,
        InstallMode::Session,
    );
    assert!(result.is_err(), "expected --mode session to refuse an unparseable template");
    assert!(
        result.unwrap_err().contains("--mode session"),
        "the error must name the flag it could not honour"
    );

    let (_temp2, ws2, defaults2) = fixture("{not json");
    init(&ws2, &defaults2, InstallMode::Default);
    assert_eq!(
        fs::read_to_string(ws2.join(".loom").join("config.json")).unwrap(),
        "{not json",
        "a default install still falls back to the verbatim copy"
    );
}

#[test]
fn session_mode_rebuilds_an_unparseable_existing_config() {
    let (_temp, workspace, defaults) = fixture(TEMPLATE);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(workspace.join(".loom").join("config.json"), "{torn write").unwrap();

    init(&workspace, &defaults, InstallMode::Session);
    let config = read_config(&workspace);
    assert_eq!(config["terminals"], serde_json::json!([]));
    assert_eq!(config["mode"], serde_json::json!("session"));
    // The pre-existing rescue copy behaviour is unchanged.
    assert!(workspace.join(".loom").join("config.json.bak").exists());
}
