//! Issue #9533: the review-stall watchdog judges work liveness (log AND
//! session transcript), not log mtime alone, and never re-dispatches while a
//! roll/drain is armed. Hazards documented atop `tests.rs` apply here. Also
//! holds the #11429 regression for the #7649 PrSet handoff.

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

/// Issue #11429 regression (#7649): the stalled sweep sampled its PR, so
/// `cancel()` journals a `landed` outcome just before the recovery
/// re-dispatch. The open-PR guard runs before the landed-journal check, so the
/// recovery still sees `OpenPrDispatchError` and converts to `PrSet([9200])`.
#[test]
#[serial]
fn review_stall_recovery_converts_to_prset_after_a_landed_cancel() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_review_stall_registry(ws, "9200", 0);

    // 1. Dispatch a sweep for issue 9100 and mark it past startup with a
    //    worktree carrying uncommitted bytes — the recovery conversion
    //    below must leave this file untouched (never clean/reset the
    //    worktree, per the issue's explicit constraint).
    let out = reg
        .dispatch(&SweepKind::Issue(9100), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS), "fixture child should start");
    let wt = ws.join(".loom").join("worktrees").join("issue-9100");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("dirty.txt"), "uncommitted Builder output\n").unwrap();
    let cp = ws.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("issue-9100.json"), r#"{"phase":"judge","pr_number":9200}"#).unwrap();
    let started_at = reg.entries.get(&out.sweep_id).unwrap().started_at;
    std::fs::OpenOptions::new()
        .write(true)
        .open(cp.join("issue-9100.json"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(2))
        .unwrap();
    reg.sample_phase_transition(&out.sweep_id, &SweepKind::Issue(9100), started_at);
    assert_eq!(reg.sampled_pr_number(&out.sweep_id), Some(9200));

    // 2. Generous timeout ⇒ the freshly-written log is not idle ⇒ healthy.
    assert_eq!(
        reg.review_stall_watchdog_once(Duration::from_secs(3600)),
        0,
        "a sweep still emitting log output is not disturbed"
    );

    // 3. Zero timeout forces the stall verdict. The Issue-keyed recovery
    //    re-dispatch hits the real #4123 open-PR guard (PR #9200 is
    //    open) and is refused — the fix converts that refusal into a
    //    `PrSet([9200])` dispatch instead of stranding the recovery.
    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);

    let tel =
        std::fs::read_to_string(reg.config().resolve_outcome_telemetry_path()).unwrap_or_default();
    assert!(
        tel.contains(r#""disposition":"landed""#) && tel.contains(r#""pr_number":9200"#),
        "the cancelled sweep journalled a landing: {tel}"
    );
    assert_eq!(restarts, 1, "the conversion counts as one recovered sweep");

    // No Issue-kind sweep for 9100 is running — no Builder/Curator was
    // re-spawned for it.
    assert!(
        running_issue_sweep_id(&reg, 9100).is_none(),
        "the Issue-keyed recovery must not run — its open PR already exists"
    );
    // A PrSet([9200]) sweep IS running — Judge/Doctor -> Merge picks up
    // the existing PR directly.
    let prset_id = running_prset_sweep_id(&reg, &[9200])
        .expect("the recovery must convert to a PrSet([9200]) dispatch");

    // Bounded retry bookkeeping unchanged: the single allowed attempt for
    // issue 9100 is consumed exactly like an ordinary Issue-kind restart.
    assert!(reg.review_stall_retried.contains(&9100), "issue marked retried (bounded)");

    // The worktree — and its uncommitted bytes — must be completely
    // untouched: no clean, no reset.
    assert_eq!(
        std::fs::read_to_string(wt.join("dirty.txt")).unwrap(),
        "uncommitted Builder output\n",
        "the PrSet conversion must never touch the issue's worktree"
    );

    // The issue's claim was released by `cancel()` (loom:building removed;
    // the open PR keeps loom:issue off, #11852), but no SECOND
    // claim flip (loom:issue -> loom:building) happened for 9100 — the
    // guard refused before any label mutation, so the original claim
    // (at the initial `dispatch()` above) is the ONLY claim flip issue
    // 9100 ever sees.
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 9100 --remove-label loom:building")
            && !calls.contains("issue edit 9100 --remove-label loom:building --add-label"),
        "cancel() must release the claim without re-queueing beside PR #9200; got: {calls:?}"
    );
    let claim_count = calls
        .lines()
        .filter(|l| {
            l.contains("issue edit 9100 --remove-label loom:issue --add-label loom:building")
        })
        .count();
    assert_eq!(
        claim_count, 1,
        "only the ORIGINAL dispatch may claim issue 9100 — no second reclaim on the refused \
             recovery; got: {calls:?}"
    );

    // Cleanup: cancel the lingering PrSet child + release its PR lock.
    let _ = reg.cancel(&prset_id, Duration::from_secs(2));
    std::env::remove_var("LOOM_REPO");
}
