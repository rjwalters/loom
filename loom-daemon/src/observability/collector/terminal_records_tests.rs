//! Unit tests for the `terminal_records` `repo` normalization (Issue #9442)
//! — in their own sibling module because `tests.rs` is pinned by the
//! file-size ratchet (`scripts/check-file-size-budget.sh`).

use super::*;
use crate::telemetry::TelemetryRecord;

/// Local copy of `tests::exited_event` — that fixture is private to the
/// sibling `tests` module, and this one pins only the fields the #9442
/// normalization reads.
fn exited_event(issue: u32, exit_code: Option<i32>, duration_sec: i64) -> Event {
    Event::SweepExited {
        issue,
        exit_code,
        duration_sec,
        no_progress: false,
        death_class: None,
        repo: Some("/repos/loom".to_string()),
    }
}

#[test]
fn terminal_records_resolve_a_path_shaped_repo_from_the_git_remote() {
    // Issue #9442 AC: a workspace whose directory name has nothing to do with
    // the repo still emits `owner/name` — resolved locally from the path's
    // git remote, never the path itself.
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    run(&["init", "-q"]);
    run(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/name-from-remote.git",
    ]);

    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &exited_event(9442, Some(0), 60),
        9442,
        dir.path().to_str().unwrap(),
        RepoVisibility::Private,
        &mut dispatches,
    );
    let outcome = records
        .iter()
        .find_map(|r| match r {
            TelemetryRecord::SweepOutcome(o) => Some(o),
            _ => None,
        })
        .expect("a terminal event yields a sweep.outcome record");
    assert_eq!(outcome.repo.as_deref(), Some("owner/name-from-remote"));
    assert!(!outcome.repo_unresolved);
    let completed = records
        .iter()
        .find_map(|r| match r {
            TelemetryRecord::SweepCompleted(c) => Some(c),
            _ => None,
        })
        .expect("a terminal event yields a sweep.completed record");
    assert_eq!(completed.repo.as_deref(), Some("owner/name-from-remote"));
}

#[test]
fn terminal_records_omit_an_unresolvable_repo_and_stamp_repo_unresolved() {
    // Issue #9442: a path that yields no slug leaves `repo` absent (never the
    // path) with `repo_unresolved: true` on the outcome record.
    let mut dispatches = HashMap::new();
    let records = map_event_to_records(
        &exited_event(9443, Some(0), 60),
        9443,
        "/definitely/not/a/repo-9443",
        RepoVisibility::Private,
        &mut dispatches,
    );
    let outcome = records
        .iter()
        .find_map(|r| match r {
            TelemetryRecord::SweepOutcome(o) => Some(o),
            _ => None,
        })
        .expect("a terminal event yields a sweep.outcome record");
    assert!(
        outcome.repo.is_none(),
        "an unresolved repo must be absent, not a path: {:?}",
        outcome.repo
    );
    assert!(outcome.repo_unresolved);
}
