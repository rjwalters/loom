//! The precheck as each managed-checkout fetch site sees it: below the floor
//! the fetch is not attempted (the clone's `origin/main` does not move) and
//! the skip is not reported as a git failure.

use super::test_override::with_free_gb;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare origin with one commit on `main`, a clone of it, and then a second
/// commit pushed to origin so the clone is one fetch behind.
fn clone_one_fetch_behind() -> (tempfile::TempDir, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "--initial-branch=main"]);
    let seed = tempfile::tempdir().unwrap();
    git(seed.path(), &["init", "-q", "--initial-branch=main"]);
    std::fs::write(seed.path().join("f"), "v1\n").unwrap();
    git(seed.path(), &["add", "."]);
    git(seed.path(), &["commit", "-q", "-m", "one"]);
    git(seed.path(), &["remote", "add", "origin", origin.path().to_str().unwrap()]);
    git(seed.path(), &["push", "-q", "origin", "main"]);

    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &[
            "clone",
            "-q",
            origin.path().to_str().unwrap(),
            clone.path().to_str().unwrap(),
        ],
    );

    std::fs::write(seed.path().join("f"), "v2\n").unwrap();
    git(seed.path(), &["commit", "-q", "-am", "two"]);
    git(seed.path(), &["push", "-q", "origin", "main"]);
    (origin, clone)
}

fn tracking(clone: &Path) -> String {
    git(clone, &["rev-parse", "refs/remotes/origin/main"])
}

#[test]
fn main_health_gate_prep_skips_as_low_disk_without_fetching() {
    use crate::main_health_gate::{
        prepare_workspace_to_origin_main, PrepOutcome, UnevaluatedClass,
    };
    let (_origin, clone) = clone_one_fetch_behind();
    let before = tracking(clone.path());

    let outcome = with_free_gb(1, || prepare_workspace_to_origin_main(clone.path()));
    match outcome {
        PrepOutcome::Skip { class, reason } => {
            assert_eq!(class, UnevaluatedClass::LowDisk, "never GitFailure: {reason}");
            assert!(reason.contains("below"), "{reason}");
        }
        other => panic!("expected a LowDisk skip, got {other:?}"),
    }
    assert_eq!(tracking(clone.path()), before, "no fetch ran below the floor");

    // Above the floor the same checkout fetches and becomes Ready.
    let outcome = with_free_gb(10_000, || prepare_workspace_to_origin_main(clone.path()));
    assert_eq!(outcome, PrepOutcome::Ready);
    assert_ne!(tracking(clone.path()), before, "the fetch ran above the floor");
}

#[test]
fn daemon_update_sync_proceeds_on_local_head_without_fetching() {
    use crate::daemon_update::args::Args;
    use crate::daemon_update::sync::{sync_with_origin, SyncState};
    let (_origin, clone) = clone_one_fetch_behind();
    let before = tracking(clone.path());
    let args = Args::parse(&["--check".to_string()]);
    let mut state = SyncState::default();

    let proceed = with_free_gb(1, || sync_with_origin(clone.path(), &args, &mut state));
    assert!(proceed, "a skipped fetch is not an abort");
    assert_eq!(tracking(clone.path()), before, "no fetch ran below the floor");
    assert_eq!(state.origin_behind_count, 0, "behind count stays unknown");
}
