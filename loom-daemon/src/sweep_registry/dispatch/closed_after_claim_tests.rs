//! Coverage for #11304: the 2.5 closed-issue guard stays fail-open, but a
//! claim whose post-flip re-check finds the issue closed is released without
//! spawning a builder (and without re-adding `loom:issue`).
//!
//! Sibling file (declared from `dispatch.rs`) because `dispatch/tests.rs` is
//! over the file-size ratchet threshold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

fn make_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// Registry whose fake `gh` answers the issue-state probe with exit 1 (a
/// lookup failure: the fail-open arm) until the `loom:building` flip has been
/// issued, and with `post_flip_stdout`/`post_flip_exit` afterwards. Returns
/// `(registry, gh_log, spawn_marker)`.
fn registry(
    ws: &Path,
    post_flip_stdout: &str,
    post_flip_exit: i32,
) -> (SweepRegistry, PathBuf, PathBuf) {
    registry_with_edit_exit(ws, post_flip_stdout, post_flip_exit, 0)
}

/// As [`registry`], but every `issue edit --remove-label loom:building` exits
/// `remove_exit` (non-zero simulates a failed claim release).
fn registry_with_edit_exit(
    ws: &Path,
    post_flip_stdout: &str,
    post_flip_exit: i32,
    remove_exit: i32,
) -> (SweepRegistry, PathBuf, PathBuf) {
    touch_sweep_command(ws);
    let gh_log = ws.join("gh-invocations.log");
    let flipped = ws.join("flipped");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" && \"$*\" == *--add-label\\ loom:building* ]]; then\n\
         touch \"{flipped}\"\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" && \"$*\" == *--remove-label\\ loom:building* ]]; then\n\
         exit {remove_exit}\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
         if [[ ! -e \"{flipped}\" ]]; then exit 1; fi\n\
         printf '%s\\n' '{state}'\n\
         exit {exit}\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = gh_log.display(),
        flipped = flipped.display(),
        state = post_flip_stdout,
        exit = post_flip_exit,
        remove_exit = remove_exit,
    );
    make_executable(&fake_gh, &script);
    let spawn_marker = ws.join("spawn-called");
    let spawn = ws.join(".loom/scripts/spawn-claude.sh");
    std::fs::create_dir_all(spawn.parent().unwrap()).unwrap();
    make_executable(
        &spawn,
        &format!("#!/usr/bin/env bash\necho spawned > '{}'\nexit 0\n", spawn_marker.display()),
    );
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log, spawn_marker)
}

fn edit_calls(gh_log: &Path) -> Vec<String> {
    std::fs::read_to_string(gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("issue edit"))
        .map(String::from)
        .collect()
}

#[test]
#[serial]
fn closed_after_claim_releases_the_claim_without_spawning() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, gh_log, spawn_marker) = registry(ws, &state_probe_json("closed", false), 0);

    let err = reg
        .dispatch(&SweepKind::Issue(11304), None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("post-claim re-verification"), "{err:#}");
    assert!(!spawn_marker.exists(), "no builder may be spawned for a closed issue");
    assert!(reg.entries.is_empty(), "no registry entry for a released claim");
    assert!(!ws.join(".loom/locks/issues/11304").exists(), "claim lock must be released");

    let edits = edit_calls(&gh_log);
    assert!(
        edits
            .iter()
            .any(|l| l.contains("--remove-label loom:building")),
        "the loom:building claim must be removed: {edits:?}"
    );
    assert!(
        !edits.iter().any(|l| l.contains("--add-label loom:issue")),
        "loom:issue must never be re-added to a closed issue: {edits:?}"
    );
}

#[test]
#[serial]
fn open_after_claim_proceeds_to_spawn() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, gh_log, spawn_marker) = registry(ws, &state_probe_json("open", false), 0);

    let outcome = reg
        .dispatch(&SweepKind::Issue(11305), None, None, None, None)
        .unwrap();
    assert!(outcome.was_new);
    assert_child_wrote(&spawn_marker, "spawned");
    assert!(
        !edit_calls(&gh_log)
            .iter()
            .any(|l| l.contains("--add-label loom:issue")),
        "an open issue's claim must not be released"
    );
}

#[test]
#[serial]
fn post_claim_probe_error_stays_fail_open() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _gh_log, spawn_marker) = registry(ws, "", 1);

    let outcome = reg
        .dispatch(&SweepKind::Issue(11306), None, None, None, None)
        .unwrap();
    assert!(outcome.was_new);
    assert_child_wrote(&spawn_marker, "spawned");
}

#[test]
#[serial]
fn failed_label_release_is_reported_not_swallowed() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _gh_log, spawn_marker) =
        registry_with_edit_exit(ws, &state_probe_json("closed", false), 0, 1);

    let err = reg
        .dispatch(&SweepKind::Issue(11307), None, None, None, None)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("FAILED"), "restore failure must be surfaced: {msg}");
    assert!(
        !msg.contains("claim was released"),
        "must not claim a release that did not happen: {msg}"
    );
    assert!(!spawn_marker.exists(), "no builder may be spawned for a closed issue");
    assert!(reg.entries.is_empty());
    assert!(
        !ws.join(".loom/locks/issues/11307").exists(),
        "local claim lock is still unwound"
    );
}
