//! Issue #9979 follow-up: a host-mode session container freezes the profile's
//! hook-control files (`hooks.json`, `config.toml`, `loom-codex-hooks.json`)
//! as read-only binds, and `session start` creates any missing one first.

use super::*;

#[test]
fn host_session_profile_controls_are_the_private_session_set() {
    // One set for both session kinds, and the posture gate reads the same
    // destinations this argv binds.
    assert_eq!(PROFILE_CONTROLS, ["hooks.json", "config.toml", "loom-codex-hooks.json"]);
    for name in PROFILE_CONTROLS {
        assert_eq!(profile_control_destination(name), format!("/home/loom/.codex-profile/{name}"));
    }
}

#[test]
fn ensure_profile_controls_creates_only_missing_placeholders() {
    let profile = tempfile::tempdir().unwrap();
    std::fs::write(profile.path().join("config.toml"), "[hooks.state]\n").unwrap();
    ensure_profile_controls(profile.path()).unwrap();
    let read = |name: &str| std::fs::read_to_string(profile.path().join(name)).unwrap();
    assert_eq!(read("config.toml"), "[hooks.state]\n", "an existing file is left alone");
    assert_eq!(read("hooks.json"), "{\"hooks\":{}}\n");
    assert_eq!(read("loom-codex-hooks.json"), "{}\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(profile.path().join("hooks.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "placeholders are private to the owner");
    }
    // The placeholder receipt pins nothing (no command, no trust baseline).
    assert!(crate::tokens_pool::codex_hooks::trust_baseline(
        &profile.path().join("loom-codex-hooks.json")
    )
    .is_none());
    // Idempotent.
    ensure_profile_controls(profile.path()).unwrap();
    assert_eq!(read("hooks.json"), "{\"hooks\":{}}\n");
}

#[test]
#[serial]
fn start_creates_the_profile_controls_before_the_container() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    for name in PROFILE_CONTROLS {
        assert!(root.path().join("alice").join(name).is_file(), "{name}");
    }
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}
