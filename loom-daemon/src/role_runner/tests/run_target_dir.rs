//! #8370: a role tick's Loom-owned `CARGO_TARGET_DIR` (planned by the runner,
//! created by the spawn) is gone once the tick returns, on every outcome.

use super::*;

/// The child creates the dir it was handed and records its path, then exits
/// with `exit`. Returns the outcome and the path the child saw.
fn tick(exit: i32) -> (RoleTickOutcome, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let seen = root.join("seen-run-target-dir");
    let script = write_fake_script(
        &root.join("bin"),
        "spawn-worker.sh",
        &format!(
            "mkdir -p \"$LOOM_RUN_TARGET_DIR\" && echo x > \"$LOOM_RUN_TARGET_DIR/artifact\" && \
             printf '%s' \"$LOOM_RUN_TARGET_DIR\" > '{}'\nexit {exit}",
            seen.display()
        ),
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(root);
    let outcome = run_role_with_timeout(
        &script,
        root,
        &ws.gh,
        "doctor",
        "/loom:doctor",
        root.join("logs"),
        Duration::from_secs(30),
        "",
        "default",
        "",
        "default",
        None,
        None,
        None,
        None,
    );
    let dir = PathBuf::from(fs::read_to_string(&seen).unwrap());
    (outcome, dir, tmp)
}

#[test]
#[serial]
fn a_finished_role_tick_removes_its_run_target_dir_on_every_outcome() {
    let _env = ClearedLoomRuntimeEnv::new();
    for (exit, want_success) in [(0, true), (3, false)] {
        let (outcome, dir, tmp) = tick(exit);
        assert_eq!(matches!(outcome, RoleTickOutcome::Success), want_success, "{outcome:?}");
        assert!(crate::run_target_dir::is_run_target_dir(&dir), "{}", dir.display());
        assert!(dir.starts_with(crate::run_target_dir::targets_root(tmp.path())));
        assert!(!dir.exists(), "run dir must be removed at run end (exit {exit})");
    }
}
