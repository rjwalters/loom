//! Issue #9533: the review-stall watchdog judges work liveness (log AND
//! session transcript), not log mtime alone, and never re-dispatches while a
//! roll/drain is armed. Hazards documented atop `tests.rs` apply here.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::time::Duration;
use tempfile::tempdir;

fn past_startup(ws: &Path, issue: u32) {
    std::fs::create_dir_all(
        ws.join(".loom")
            .join("worktrees")
            .join(format!("issue-{issue}")),
    )
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
