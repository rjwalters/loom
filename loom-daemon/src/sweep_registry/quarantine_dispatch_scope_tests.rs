//! End-to-end regression coverage for Issue #8716: crash classification and
//! account-exhaustion evidence must be scoped to the CURRENT sweep's log
//! region, not the whole accumulated per-issue log file, for both the
//! checkpoint ("normal"/`Crashed`) reaper path and the checkpoint-less
//! (#5697) path a freshly-dispatched-but-uncheckpointed OR an
//! adopted/recovered sweep takes.
//!
//! Lives in its own sibling module rather than `quarantine.rs`'s `mod
//! tests`: that file is already over the file-size ratchet's threshold and
//! therefore frozen at its current size (see
//! `.loom/docs/file-size-policy.md`), and this mirrors the existing
//! `quarantine_empty_pool_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::sweep_registry::test_support::{
    backoff_registry, insert_dead_running_with_log, seed_token_pool, write_checkpoint,
};
use tempfile::tempdir;

/// The #8675/#8676 incident this issue reports, reproduced verbatim in
/// shape: a per-issue log whose EARLIER dispatch (a Claude attempt) died on
/// a genuine account-exhaustion signature for `agent-old`, reused for a
/// brand-new dispatch that captured a DIFFERENT account (`agent-new`) and
/// then died on a distinct, unrelated native-model/config failure — after
/// reaching `# CLAUDE_CLI_START` (so this is not itself a pre-flight death;
/// it exercises the account-exhaustion quarantine call site, not the #4386
/// pre-flight streak).
fn stale_exhaustion_then_new_distinct_failure(issue: u32) -> String {
    format!(
        "==== loom-daemon dispatch: sweep-issue-{issue}-0 ====\n\
         spawn-claude: using OAuth account 'agent-old' (mode=ranking)\n\
         # CLAUDE_CLI_START\n\
         Claude: You've hit your weekly limit — try again later\n\
         ==== loom-daemon dispatch: sweep-issue-{issue}-1 ====\n\
         spawn-claude: using OAuth account 'agent-new' (mode=ranking)\n\
         # CLAUDE_CLI_START\n\
         bare model differs from the selected profile; use provider/model or define a model profile\n"
    )
}

/// Checkpoint-less reaper path (reaper.rs's #5697 branch): the shape a death
/// that never wrote a phase checkpoint takes — a freshly-dispatched sweep
/// killed before its first checkpoint write, or an adopted/reconstructed
/// entry (post daemon-restart) observed dead by `reap_once` before any
/// checkpoint for this run exists.
#[test]
fn checkpoint_less_path_does_not_bad_mark_the_new_account_on_stale_exhaustion() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-old");
    seed_token_pool(dir.path(), "agent-new");

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        8716,
        0,
        "agent-new",
        &stale_exhaustion_then_new_distinct_failure(8716),
    );
    registry.reap_once();

    assert!(
        !bad_tokens::is_bad(dir.path(), "agent-new"),
        "the CURRENT dispatch's account must not be bad-marked on an OLDER dispatch's \
         exhaustion text bleeding through the reused per-issue log (#8716)"
    );
    assert_eq!(
        registry.insta_crash_count(8716),
        1,
        "a genuinely new, distinct failure must still charge the ISSUE's own insta-crash tally \
         — before the fix this was swallowed by the stale exhaustion match instead"
    );

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let record = records
        .iter()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the death must be journaled");
    assert_ne!(
        record.crash_classification.as_deref(),
        Some("account-exhausted:rate-limited"),
        "must not inherit the OLDER dispatch's exhaustion signature: {:?}",
        record.crash_classification
    );
}

/// The same fixture through the "normal" checkpoint-present (`Crashed`)
/// branch (reaper.rs, the `if checkpoint.exists()` arm): a checkpoint exists
/// but predates this run's `started_at` (a stale artifact of an earlier
/// dispatch, not progress made by the current one), so the reaper still
/// falls through to the same insta-crash accounting as the checkpoint-less
/// path above — this asserts the fix covers BOTH branches, not just one.
#[test]
fn checkpoint_present_path_does_not_bad_mark_the_new_account_on_stale_exhaustion() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-old");
    seed_token_pool(dir.path(), "agent-new");

    // Written BEFORE the run below, so its mtime predates `started_at` and
    // `checkpoint_written_by_run` reports no progress from THIS run.
    write_checkpoint(&registry, 8717, "building");
    std::thread::sleep(std::time::Duration::from_millis(20));

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        8717,
        0,
        "agent-new",
        &stale_exhaustion_then_new_distinct_failure(8717),
    );
    registry.reap_once();

    assert!(
        !bad_tokens::is_bad(dir.path(), "agent-new"),
        "the CURRENT dispatch's account must not be bad-marked on an OLDER dispatch's \
         exhaustion text bleeding through the reused per-issue log (#8716)"
    );
    assert_eq!(
        registry.insta_crash_count(8717),
        1,
        "a genuinely new, distinct failure must still charge the ISSUE's own insta-crash tally"
    );

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let record = records
        .iter()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the death must be journaled");
    assert_ne!(
        record.crash_classification.as_deref(),
        Some("account-exhausted:rate-limited"),
        "must not inherit the OLDER dispatch's exhaustion signature: {:?}",
        record.crash_classification
    );
}

/// Regression guard (both call sites, checkpoint-less path): scoping must
/// narrow, not disable, exhaustion detection — a genuine exhaustion signature
/// WITHIN the current dispatch region still marks the spawn account bad and
/// leaves the issue's own tally untouched (the #4122 carve-out).
#[test]
fn genuine_exhaustion_within_the_current_dispatch_still_marks_the_account_bad() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");

    insert_dead_running_with_log(
        &mut registry,
        8718,
        0,
        "agent-9",
        "==== loom-daemon dispatch: sweep-issue-8718-0 ====\n\
         spawn-claude: using OAuth account 'agent-9' (mode=ranking)\n\
         # CLAUDE_CLI_START\n\
         Claude: You've hit your weekly limit — try again later\n",
    );
    registry.reap_once();

    assert!(
        bad_tokens::is_bad(dir.path(), "agent-9"),
        "a genuine in-region exhaustion signature must still mark the spawn account bad"
    );
    assert_eq!(
        registry.insta_crash_count(8718),
        0,
        "an exhaustion death must not also charge the issue's own insta-crash tally (#4122)"
    );
}

/// Missing/unreadable logs must degrade gracefully — no panic, no false
/// account-exhaustion match, and the issue's tally still accrues normally
/// like any other unreadable-log insta-crash.
#[test]
fn missing_log_degrades_gracefully_without_bad_marking_anything() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");

    let sweep_id = insert_dead_running_with_log(&mut registry, 8719, 0, "agent-9", "placeholder");
    let log_path = registry
        .entries
        .get(&sweep_id)
        .expect("entry inserted above")
        .log_path
        .clone();
    std::fs::remove_file(&log_path).unwrap();

    registry.reap_once();

    assert!(
        !bad_tokens::is_bad(dir.path(), "agent-9"),
        "a missing log must never be treated as exhaustion evidence"
    );
    assert_eq!(registry.insta_crash_count(8719), 1);
}
