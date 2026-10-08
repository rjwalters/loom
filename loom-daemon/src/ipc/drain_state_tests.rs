//! Issue #9588: drain origin, the operator timed-out hold, escalation of an
//! in-progress drain, and the durable operator-stop record.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::drain_supervisor::drain_timeout_hold_note;
use super::*;

fn started_deadline(b: DrainBegin) -> chrono::DateTime<Utc> {
    match b {
        DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    }
}

/// AC1: a timed-out operator drain (then-exit AND relaunch) stays paused and
/// active, the deadline is cleared, the same supervisor generation keeps
/// running, and the terminal action still fires once in-flight reaches zero.
#[test]
fn an_operator_timeout_holds_dispatch_paused() {
    for then_exit in [true, false] {
        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(1), false, then_exit);
        let gen = drain.generation();
        assert_eq!(drain.snapshot().origin, DrainOrigin::Operator);
        let stragglers = vec!["sweep-a (issue #1)".to_string()];
        drain.hold_after_timeout(drain_timeout_hold_note(then_exit, &stragglers));

        assert!(drain.is_draining(), "dispatch must stay paused (then_exit={then_exit})");
        let snap = drain.snapshot();
        assert!(snap.active && snap.timed_out);
        assert_eq!(snap.deadline, None, "nothing left to time out");
        assert_eq!(drain.generation(), gen, "the same supervisor keeps supervising");
        let note = snap.note.unwrap();
        assert!(note.contains("stays PAUSED"), "{note}");
        assert!(note.contains("sweep-a (issue #1)"), "{note}");
        assert!(!note.contains("dispatch resumed"), "{note}");
        // With the deadline cleared the supervisor only ever continues or
        // completes — it can never reach a second refusal.
        assert_eq!(evaluate_drain_tick(2, false, false), DrainTick::Continue);
        assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);

        // Only an explicit abort resumes dispatch.
        assert!(drain.abort());
        assert!(!drain.is_draining());
        assert!(!drain.snapshot().timed_out);
    }
}

/// AC2: `--force-after-timeout` escalates a first-attempt drain; the deadline
/// only moves earlier.
#[test]
fn force_escalates_a_first_attempt_drain_and_only_pulls_the_deadline_in() {
    let drain = DrainState::new();
    let deadline = started_deadline(drain.begin(Duration::from_secs(1800), false, true));
    match drain.begin(Duration::from_secs(60), true, false) {
        DrainBegin::AlreadyDraining {
            force_escalated,
            active_then_exit,
            ..
        } => {
            assert!(force_escalated);
            assert!(active_then_exit, "then-exit is never downgraded");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    let snap = drain.snapshot();
    assert!(snap.force_after_timeout);
    let new_deadline = snap.deadline.unwrap();
    assert!(new_deadline < deadline, "--timeout 60 pulls a 30-minute deadline in");
    assert!(new_deadline <= Utc::now() + chrono::Duration::seconds(60));
}

/// AC2: escalating a drain that already timed out (held paused) forces on the
/// very next tick.
#[test]
fn force_escalates_a_timed_out_hold_to_now() {
    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(1), false, true);
    drain.hold_after_timeout("held".to_string());
    let before = Utc::now();
    match drain.begin(Duration::from_secs(1800), true, true) {
        DrainBegin::AlreadyDraining {
            force_escalated, ..
        } => assert!(force_escalated),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    let snap = drain.snapshot();
    let d = snap.deadline.expect("a forced hold re-arms a deadline");
    assert!(d <= Utc::now() && d >= before, "forces on the next tick");
    assert_eq!(evaluate_drain_tick(2, true, snap.force_after_timeout), DrainTick::TimedOutForce);
}

/// AC1: an operator request promotes an auto-update roll one way, and the
/// auto-updater can then neither label nor abort it.
#[test]
fn an_operator_request_promotes_a_pause_roll() {
    let drain = DrainState::new();
    let _ = drain.begin_as(Duration::from_secs(1800), false, false, DrainOrigin::PauseRoll);
    drain.set_roll_target(Some("v1".to_string()));
    assert_eq!(drain.snapshot().roll_target.as_deref(), Some("v1"));

    match drain.begin(Duration::from_secs(1800), false, false) {
        DrainBegin::AlreadyDraining {
            origin_promoted, ..
        } => assert!(origin_promoted),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    let snap = drain.snapshot();
    assert_eq!(snap.origin, DrainOrigin::Operator);
    assert_eq!(snap.roll_target, None, "no longer a supersedable roll");

    // An auto-update request never demotes it.
    match drain.begin_as(Duration::from_secs(1800), false, false, DrainOrigin::PauseRoll) {
        DrainBegin::AlreadyDraining {
            origin_promoted, ..
        } => assert!(!origin_promoted),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    drain.set_roll_target(Some("v2".to_string()));
    assert_eq!(drain.snapshot().roll_target, None);
    assert!(!drain.abort_pause_roll(), "the auto-updater cannot end an operator drain");
    assert!(drain.is_draining());
}

#[test]
fn the_auto_updater_can_still_end_its_own_roll() {
    let drain = DrainState::new();
    let _ = drain.begin_as(Duration::from_secs(1800), false, false, DrainOrigin::PauseRoll);
    assert!(drain.abort_pause_roll());
    assert!(!drain.is_draining());
}

const MARKER_BODY: &str = "started_at=2026-09-30T00:00:00Z\npid_file=/p\n";

/// AC5/AC6: a then-exit drain records the operator stop (moving the marker
/// aside), and `--abort-drain` restores it.
#[test]
fn a_then_exit_drain_records_the_stop_and_abort_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, MARKER_BODY).unwrap();
    let drain = DrainState::new().with_stop_marker(marker.clone());
    assert!(!drain.is_draining(), "no record ⇒ no startup hold");

    let _ = drain.begin(Duration::from_secs(60), false, true);
    assert!(crate::operator_stop::is_recorded(&marker));
    assert!(!marker.exists(), "the watchdog must see no autonomy-desired marker");

    assert!(drain.abort());
    assert!(!crate::operator_stop::is_recorded(&marker));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), MARKER_BODY);
}

/// AC5: a relaunch drain escalated to then-exit records the stop too; a plain
/// relaunch drain never does.
#[test]
fn only_a_then_exit_drain_records_the_stop() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, MARKER_BODY).unwrap();
    let drain = DrainState::new().with_stop_marker(marker.clone());
    let _ = drain.begin(Duration::from_secs(60), false, false);
    assert!(!crate::operator_stop::is_recorded(&marker), "a relaunch is not a stop");
    let _ = drain.begin(Duration::from_secs(60), false, true);
    assert!(crate::operator_stop::is_recorded(&marker), "the escalation is a stop");
}

/// AC5: a daemon that starts while an operator stop is on record comes up
/// HELD; a drain request replaces the hold with a real drain, and an abort
/// releases it and restores the marker.
#[test]
fn a_recorded_stop_holds_dispatch_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, MARKER_BODY).unwrap();
    crate::operator_stop::record(&marker, "restart --drain --then-exit").unwrap();

    let drain = DrainState::new().with_stop_marker(marker.clone());
    assert!(drain.is_draining(), "a supervised relaunch must not resume dispatch");
    let snap = drain.snapshot();
    assert!(snap.startup_hold && snap.active);
    assert!(snap.note.unwrap().contains("HELD at startup"));

    // A drain request replaces the (supervisor-less) hold with a real drain.
    assert!(matches!(
        drain.begin(Duration::from_secs(60), false, true),
        DrainBegin::Started { .. }
    ));
    assert!(!drain.snapshot().startup_hold);

    assert!(drain.abort());
    assert!(!drain.is_draining());
    assert!(!crate::operator_stop::is_recorded(&marker));
    assert!(marker.exists(), "the moved-aside marker is restored");
}

/// AC6: `--abort-drain` with no drain active still clears a stale record.
#[test]
fn abort_with_no_drain_clears_a_stale_record() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    let drain = DrainState::new().with_stop_marker(marker.clone());
    std::fs::write(&marker, MARKER_BODY).unwrap();
    crate::operator_stop::record(&marker, "stale").unwrap();
    assert!(!drain.abort(), "no drain was in progress");
    assert!(!crate::operator_stop::is_recorded(&marker));
    assert!(marker.exists());
}

/// In-memory states never touch the filesystem (so a unit test can never move
/// an operator's real marker aside).
#[test]
fn an_in_memory_state_records_nothing() {
    let drain = DrainState::new();
    let _ = drain.begin(Duration::from_secs(60), false, true);
    assert!(drain.stop_marker.is_none());
}

// Moved from `ipc/tests.rs` (frozen by the file-size ratchet) when #9588
// changed their force-escalation expectations.

/// The `DrainState` machine: begin sets the flag and a deadline, a second
/// begin is idempotent (does not stack / move the deadline), abort clears
/// the flag and bumps the generation, and the timeout path clears + notes.
#[test]
fn test_drain_state_lifecycle() {
    let drain = DrainState::new();
    assert!(!drain.is_draining());
    assert_eq!(drain.generation(), 0);

    // begin ⇒ Started, flag set, deadline recorded, generation bumped.
    let (gen1, deadline) = match drain.begin(Duration::from_secs(120), false, false) {
        DrainBegin::Started {
            generation,
            deadline,
        } => (generation, deadline),
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(drain.is_draining());
    assert_eq!(gen1, 1);
    assert_eq!(drain.snapshot().deadline, Some(deadline));
    assert!(!drain.snapshot().force_after_timeout);

    // A second begin while already draining is idempotent: same generation,
    // same deadline, flag still set (AC edge: second drain does not stack).
    match drain.begin(Duration::from_secs(9999), true, false) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
            ..
        } => {
            assert!(!active_then_exit, "active drain is still a relaunch drain");
            assert!(!escalated, "a then_exit=false request escalates nothing");
            assert!(
                force_escalated,
                "#9588: a later --force-after-timeout escalates ANY active drain \
                     (it used to be a silent no-op for a first-attempt drain)"
            );
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(
        drain.snapshot().force_after_timeout,
        "#9588: the escalation is visible to the running supervisor"
    );
    assert_eq!(drain.generation(), gen1, "idempotent begin does not bump gen");
    assert_eq!(
        drain.snapshot().deadline,
        Some(deadline),
        "a longer requested timeout never moves the deadline LATER (#4521 / #9588)"
    );

    // abort ⇒ flag cleared, generation bumped (so a live supervisor stops),
    // note recorded.
    assert!(drain.abort());
    assert!(!drain.is_draining());
    assert_eq!(drain.generation(), gen1 + 1);
    assert!(drain.snapshot().note.unwrap().contains("aborted"));
    // abort again ⇒ no-op.
    assert!(!drain.abort());

    // timeout resolution clears + notes + bumps generation.
    let gen_before = drain.generation();
    let _ = drain.begin(Duration::from_secs(1), false, false);
    drain.resolve_timeout("timed out".to_string());
    assert!(!drain.is_draining());
    assert_eq!(drain.snapshot().note.as_deref(), Some("timed out"));
    assert!(drain.generation() > gen_before);
}

/// Issue #4521 AC1 — `then_exit` on the already-draining path is escalated
/// **one way** (relaunch → stay-down) and the outcome reported back is the
/// ACTIVE drain's terminal action, never a blind echo of the request.
#[test]
fn test_drain_then_exit_escalates_one_way() {
    // A relaunch-drain is in flight (this is the auto-update roll's shape:
    // `then_exit=false`).
    let drain = DrainState::new();
    let deadline = match drain.begin(Duration::from_secs(120), false, false) {
        DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(!drain.snapshot().then_exit);

    // An operator teardown request lands mid-roll: it must NOT be silently
    // ignored (the #4521 defect) — the active drain escalates to stay-down.
    match drain.begin(Duration::from_secs(9999), true, true) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
            ..
        } => {
            assert!(active_then_exit, "the active drain now stays down");
            assert!(escalated, "the escalation must be reported to the caller");
            assert!(force_escalated, "#9588: force escalates any active drain");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(
        drain.snapshot().then_exit,
        "the escalation must be visible to the already-running supervisor, \
             which re-reads the descriptor"
    );
    // The deadline never moves later (the request asked for 9999s).
    assert_eq!(drain.snapshot().deadline, Some(deadline));
    assert!(drain.snapshot().force_after_timeout, "#9588: force escalated too");

    // Escalating again is a no-op that still reports the truth.
    match drain.begin(Duration::from_secs(1), false, true) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            ..
        } => {
            assert!(active_then_exit);
            assert!(!escalated, "already stay-down — nothing to escalate");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }

    // A relaunch request against an active teardown drain must NOT downgrade
    // it: the reply still says "will stay down".
    match drain.begin(Duration::from_secs(1), false, false) {
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            ..
        } => {
            assert!(active_then_exit, "then-exit is never downgraded");
            assert!(!escalated);
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(drain.snapshot().then_exit);

    // After an abort, a fresh drain starts from the requested terminal
    // action again (the escalation does not leak across drains).
    assert!(drain.abort());
    match drain.begin(Duration::from_secs(30), false, false) {
        DrainBegin::Started { .. } => {}
        other => panic!("expected Started, got {other:?}"),
    }
    assert!(!drain.snapshot().then_exit, "a fresh drain honors its own then_exit");
}
