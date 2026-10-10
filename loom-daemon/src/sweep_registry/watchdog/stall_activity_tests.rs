//! Issue #9533: the review-stall watchdog judges work liveness (log AND
//! session transcript), not log mtime alone, and never re-dispatches while a
//! roll/drain is armed. Hazards documented atop `tests.rs` apply here.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::time::Duration;
use tempfile::tempdir;

/// Mark the sweep past startup by creating its worktree, backdated so the
/// worktree signal (#9533) carries no fresh evidence unless a test writes to it.
fn past_startup(ws: &Path, issue: u32) -> std::path::PathBuf {
    let wt = ws
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{issue}"));
    std::fs::create_dir_all(&wt).unwrap();
    age_path(&wt, 9000);
    wt
}

/// Backdate a file's or directory's mtime by `secs`.
fn age_path(path: &Path, secs: u64) {
    std::fs::File::open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
        .unwrap();
}

#[test]
#[serial]
fn fresh_transcript_keeps_a_log_silent_sweep_healthy() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let projects = tmp.path().join("claude");
    std::env::set_var("CLAUDE_CONFIG_DIR", &projects);
    let mut reg = hung_child_registry(&ws);
    let out = reg
        .dispatch(&SweepKind::Issue(9533), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    past_startup(&ws, 9533);

    // Age the log far past the timeout; with no transcript it is a stall.
    let log = reg.entries.get(&out.sweep_id).unwrap().log_path.clone();
    let f = std::fs::File::options().write(true).open(&log).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(9000))
        .unwrap();
    drop(f);

    // A live, freshly-appended session transcript for this sweep.
    let dir = projects
        .join("projects")
        .join(crate::transcript_tokens::project_slug(&ws));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s.jsonl"),
        "{\"c\":\"<command-name>/loom:sweep</command-name><command-args>9533 --claim-owned 9533</command-args>\"}\n",
    )
    .unwrap();

    assert_eq!(reg.review_stall_watchdog_once(Duration::from_secs(2700)), 0);
    assert!(!reg.review_stall_retried.contains(&9533), "healthy sweep must not be cancelled");

    // Same sweep, transcript gone: all signals old => Restart.
    std::fs::remove_dir_all(&projects).unwrap();
    assert_eq!(reg.review_stall_watchdog_once(Duration::from_secs(2700)), 1);
    assert!(reg.review_stall_retried.contains(&9533));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    if let Some(id) = running_issue_sweep_id(&reg, 9533) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

#[test]
#[serial]
fn armed_drain_defers_cancel_and_redispatch() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join("claude"));
    let mut reg = hung_child_registry(&ws);
    let out = reg
        .dispatch(&SweepKind::Issue(9534), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    past_startup(&ws, 9534);

    reg.close_for_roll("manifest-x", true);
    assert_eq!(reg.review_stall_watchdog_once(Duration::ZERO), 0);
    assert!(
        !reg.review_stall_retried.contains(&9534),
        "retry latch untouched while draining"
    );
    assert_eq!(
        running_issue_sweep_id(&reg, 9534).as_deref(),
        Some(out.sweep_id.as_str()),
        "the original sweep is neither cancelled nor replaced"
    );
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// #9533 signal 3: a sweep whose log is silent past the timeout but whose
/// worktree was just written (an edit, a build) is Healthy; once every signal
/// is old it still resolves to Restart, then GiveUp (bounded once).
#[test]
#[serial]
fn fresh_worktree_write_keeps_a_log_silent_sweep_healthy() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // No transcripts at all: that signal is missing, never "stalled".
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join("claude"));
    let mut reg = hung_child_registry(&ws);
    let out = reg
        .dispatch(&SweepKind::Issue(9535), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    let wt = past_startup(&ws, 9535);
    let src = wt.join("lib.rs");
    std::fs::write(&src, "// fresh edit\n").unwrap();
    let log = reg.entries.get(&out.sweep_id).unwrap().log_path.clone();
    age_path(&log, 9000);

    let timeout = Duration::from_secs(2700);
    assert_eq!(reg.review_stall_watchdog_once(timeout), 0);
    assert!(!reg.review_stall_retried.contains(&9535), "fresh worktree write => Healthy");

    // Every signal old => Restart once.
    age_path(&src, 9000);
    age_path(&wt, 9000);
    assert_eq!(reg.review_stall_watchdog_once(timeout), 1);
    assert!(reg.review_stall_retried.contains(&9535));

    // The re-dispatched sweep goes silent on every signal too => GiveUp.
    let second = running_issue_sweep_id(&reg, 9535).expect("re-dispatched");
    assert!(wait_until_alive(reg.entries.get(&second).unwrap().pid, FIXTURE_CHILD_WAIT_MS));
    age_path(&reg.entries.get(&second).unwrap().log_path.clone(), 9000);
    age_path(&wt, 9000);
    assert_eq!(reg.review_stall_watchdog_once(timeout), 0);
    assert!(reg.review_stall_gaveup.contains(&9535), "bounded: gives up on the second stall");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let _ = reg.cancel(&second, Duration::from_secs(2));
}

/// #9533: with the log unreadable and no other signal readable, the sweep is
/// left alone — missing signals never produce a stall verdict.
#[test]
#[serial]
fn no_readable_signal_is_never_a_stall() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join("claude"));
    let mut reg = hung_child_registry(&ws);
    let out = reg
        .dispatch(&SweepKind::Issue(9536), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    // A checkpoint (not a worktree) marks it past startup, so there is no
    // worktree signal; then the log goes missing.
    let ckpt = reg.config.checkpoint_dir();
    std::fs::create_dir_all(&ckpt).unwrap();
    std::fs::write(ckpt.join("issue-9536.json"), "{}").unwrap();
    let log = reg.entries.get(&out.sweep_id).unwrap().log_path.clone();
    std::fs::remove_file(&log).unwrap();

    assert_eq!(reg.review_stall_watchdog_once(Duration::ZERO), 0);
    assert!(!reg.review_stall_retried.contains(&9536), "cannot assess => leave alone");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}
