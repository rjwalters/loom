//! Issue #9533: the review-stall watchdog judges work liveness (log AND
//! session transcript), not log mtime alone, and never re-dispatches while a
//! roll/drain is armed. Hazards documented atop `tests.rs` apply here. Also
//! holds the #11429 regression for the #7649 PrSet handoff.

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

    // The issue's claim was restored by `cancel()` (loom:building ->
    // loom:issue) exactly as an ordinary recovery would, but no SECOND
    // claim flip (loom:issue -> loom:building) happened for 9100 — the
    // guard refused before any label mutation, so the original claim
    // (at the initial `dispatch()` above) is the ONLY claim flip issue
    // 9100 ever sees.
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 9100 --remove-label loom:building --add-label loom:issue"),
        "cancel() must restore the issue's claim; got: {calls:?}"
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
