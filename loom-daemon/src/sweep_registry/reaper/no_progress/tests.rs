//! Regression tests for the #8439 progress carve-out in
//! [`SweepRegistry::is_no_progress`].
//!
//! The pre-existing #4366/#4452/#6350 predicate tests (the counted shape, the
//! open-linked-PR exemption, the closed-issue exemption, both fail-open cases,
//! the `skip_label_flip` no-op, and the `SweepExited` event payload) live in
//! `reaper/tests.rs` and are deliberately left untouched by this change — they
//! are what pins "the fix NARROWS the predicate, it does not disable it".

use crate::sweep_registry::reaper::*;
use crate::sweep_registry::test_support::*;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

/// Issue #8439: a PRODUCTIVE exit-0 sweep — one the daemon watched advance
/// through real lifecycle phases — must not be charged to the insta-crash
/// quarantine tally just because the forge's *end state* happens to match
/// the #4366 parked-on-monitor signature.
///
/// This is the partial-increment shape, and it satisfied BOTH pre-fix forge
/// arms: the sweep's PR was merged (so the open-linked-PR probe truthfully
/// answers `NoneOpen`) and the issue is deliberately still open (a
/// `Part of #N` slice does not close its parent), so `issue_is_closed_or_pr`
/// truthfully answers "open". Every dispatch that landed real work therefore
/// accrued a failed attempt, and the issue quarantined itself out of the
/// queue despite making progress every single time.
///
/// **Reachability is the point of this test's shape.** The phase history is
/// NOT hand-planted: tick 1 observes a live checkpoint exactly as
/// `sample_phase_transition` does on a real daemon, then the checkpoint is
/// deleted (mirroring the sweep skill's success-path deletion) and the
/// process exits 0, so tick 2 lands in the checkpoint-less clean-exit branch
/// with nothing but the sampled history to exempt it.
///
/// The checkpoint deliberately carries NO `pr_number`: with one, the #8355
/// memo seed would turn the probe's verdict into `Open(_)` and the exit would
/// already be exempt via the #6350 carve-out, leaving this fix's new
/// `phase_history` arm untested. Absent it, the probe genuinely returns
/// `NoneOpen` and the sampled history is the ONLY thing standing between a
/// productive sweep and the tally.
#[test]
fn sampled_phase_history_exempts_a_productive_clean_exit() {
    let dir = tempdir().unwrap();
    // No open linked PR (it merged) + the issue still OPEN: the exact pair
    // that made the pre-fix predicate fire.
    let mut registry = no_progress_test_registry(dir.path(), "OPEN", "", false);

    let sweep_id = "sweep-issue-8439-partial-increment".to_string();
    // Alive on tick 1 (this process's pid is the cheapest guaranteed-live
    // one), with `started_at` in the past so the checkpoint written below
    // lands inside this run's window (#4009 `checkpoint_written_by_run`).
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(84_390),
            pid: std::process::id(),
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(84_390),
            idempotency_key: None,
            started_at: Utc::now() - chrono::Duration::seconds(600),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    // Pre-arm the per-issue dispatch backoff so the "cleared, not merely
    // never armed" assertion below is observable: the same corrected
    // `no_progress` feeds that arm, so a productive exit must actively CLEAR
    // a stale window.
    registry.record_dispatch_failure(84_390);
    assert_eq!(registry.dispatch_failure_count(84_390), 1);

    // Tick 1 — the sweep is alive and mid-lifecycle; the reaper samples the
    // phase transition off its checkpoint.
    let checkpoint_dir = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    let checkpoint = checkpoint_dir.join("issue-84390.json");
    std::fs::write(&checkpoint, r#"{"phase":"judge-done","issue":84390}"#).unwrap();
    assert_eq!(registry.reap_once(), 0, "a live sweep must not be reaped on the first tick");
    assert!(
        registry.sampled_pr_number(&sweep_id).is_none(),
        "this fixture must NOT sample a pr_number — otherwise the #8355 memo \
         seed makes the probe answer Open(_) and the new phase-history arm is \
         never exercised"
    );

    // The sweep finishes: the skill deletes the checkpoint, and the process
    // exits 0. Swap in a real, already-dead `true` child so `poll_liveness`
    // observes `exit_code == Some(0)` — the no-handle fallback yields `None`,
    // which would skip the PR probe entirely and make this test vacuous.
    std::fs::remove_file(&checkpoint).unwrap();
    let child = Command::new("true")
        .spawn()
        .expect("spawn `true` fixture child");
    registry.entries.get_mut(&sweep_id).unwrap().pid = child.id();
    registry.children.insert(sweep_id.clone(), child);
    std::thread::sleep(Duration::from_millis(50));

    // Tick 2 -> the checkpoint-less clean-exit branch this fix lives in.
    let changed = registry.reap_once();
    assert!(changed >= 1, "reap_once should observe the dead fixture child");
    let info = registry.get(&sweep_id).unwrap();
    assert!(
        matches!(info.state, SweepState::Exited { code: Some(0), .. }),
        "the fixture must reach the branch under test via a real exit-0; got: {:?}",
        info.state
    );

    assert_eq!(
        registry.insta_crash_count(84_390),
        0,
        "a sweep the daemon watched advance through a lifecycle phase must not be \
         charged to the insta-crash quarantine tally (#8439)"
    );
    assert!(
        !registry.is_quarantined(84_390),
        "a productive exit must never contribute quarantine pressure"
    );
    assert_eq!(
        registry.dispatch_failure_count(84_390),
        0,
        "the dispatch-backoff arm reads the same corrected `no_progress`, so a \
         productive exit must CLEAR the pre-armed window rather than re-arm it"
    );
    assert!(
        registry
            .dispatch_backoff_remaining(84_390, Utc::now())
            .is_none(),
        "no backoff may remain in effect after a productive exit"
    );
}

/// Issue #8439 (edge case a): the exemption keys on a NON-EMPTY sampled
/// history, not on the mere presence of a `phase_history` entry for the
/// sweep. A sweep that has an entry but nothing recorded in it is still the
/// zero-progress shape #4366 targets, so it must still count — this is the
/// half of the change that proves the predicate was narrowed, not disabled.
///
/// Pins the `!history.is_empty()` spelling against a regression to
/// `contains_key`, which would silently disable the backstop for any sweep
/// that ever touched the map.
#[test]
fn an_empty_sampled_phase_history_still_counts_as_no_progress() {
    let dir = tempdir().unwrap();
    let mut registry = no_progress_test_registry(dir.path(), "OPEN", "", false);

    let sweep_id = insert_clean_exit_running(&mut registry, 84_391, 0);
    // An entry in the map, but nothing ever observed through it.
    registry.phase_history.insert(sweep_id.clone(), Vec::new());

    registry.reap_once();

    assert_eq!(
        registry.insta_crash_count(84_391),
        1,
        "an EMPTY sampled phase history is not evidence of progress — the #4366 \
         backstop must still count this exit"
    );
    assert!(
        registry.dispatch_failure_count(84_391) >= 1,
        "and it must still arm the per-issue dispatch backoff"
    );
}
