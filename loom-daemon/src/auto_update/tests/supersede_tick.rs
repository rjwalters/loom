//! Issue #8514: what `run_tick` does when a roll is **already armed**.
//!
//! #6007 made that case an unconditional skip — correct while the armed roll is
//! still the right one to be waiting for, and wrong once a newer release has
//! overtaken it: the host kept dispatch paused for up to its whole
//! paused-dispatch budget converging on a binary that was already stale.
//!
//! This module pins both halves end to end through `run_tick`: the skip is
//! preserved verbatim for every shape that is not a superseded pending roll
//! (that is the #6007 fail-safe, and the livelock it was built to prevent lives
//! behind it), and exactly one shape — a *pending* auto-update roll whose
//! artifact has been overtaken — supersedes instead.
//!
//! A sibling module rather than more lines in `tests.rs`, per
//! `.loom/docs/file-size-policy.md` (that file is over the ratchet threshold).

use super::*;
use crate::auto_update::supersede::ArmedRoll;
use std::sync::Mutex;

/// Issue #6007 — while a roll is already armed (in particular one *retained*
/// across a refused deadline: dispatch paused, restart re-arming itself at
/// quiescence) the loop must not rebuild or re-trigger. The binary is already
/// provisioned, and a redundant `cargo build` would compete for CPU with the
/// very in-flight sweeps the pending roll is waiting on.
#[test]
fn test_run_tick_skips_while_a_roll_is_already_armed() {
    struct PendingRollTrigger {
        calls: Arc<AtomicUsize>,
    }
    impl DrainTrigger for PendingRollTrigger {
        fn trigger(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn roll_in_progress(&self) -> bool {
            true
        }
    }

    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        // Busy host — exactly the shape that made the roll go pending.
        in_flight: 3,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = PendingRollTrigger {
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no redundant rebuild");
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 0, "no redundant drain trigger");
    let snap = status.snapshot();
    assert!(
        snap.note
            .as_deref()
            .is_some_and(|n| n.contains("already armed")),
        "the skip must be explained in status, got: {:?}",
        snap.note
    );
}

// ---- #8514: supersede-not-stack --------------------------------------

/// A trigger standing in for a daemon that already has a roll armed. It reports
/// the armed roll's identity, records every `trigger_for` target, and — when
/// superseded — flips to "no roll armed" exactly as `DrainState::abort()` does.
struct ArmedTrigger {
    armed: Mutex<Option<ArmedRoll>>,
    /// Whether `supersede_roll` should claim it actually discarded a roll.
    supersede_succeeds: bool,
    targets: Arc<Mutex<Vec<Option<String>>>>,
    supersedes: Arc<Mutex<Vec<(String, String)>>>,
}

impl ArmedTrigger {
    fn new(armed: ArmedRoll) -> Self {
        Self {
            armed: Mutex::new(Some(armed)),
            supersede_succeeds: true,
            targets: Arc::new(Mutex::new(Vec::new())),
            supersedes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn pending(target: Option<&str>) -> Self {
        Self::new(ArmedRoll {
            target: target.map(str::to_string),
            pending: true,
            then_exit: false,
            refusals: 1,
        })
    }
}

impl DrainTrigger for ArmedTrigger {
    fn trigger(&self) -> bool {
        true
    }
    fn trigger_for(&self, target: Option<&str>) -> bool {
        self.targets
            .lock()
            .unwrap()
            .push(target.map(str::to_string));
        true
    }
    fn roll_in_progress(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
    fn supersede_roll(&self, from: &str, to: &str) -> bool {
        self.supersedes
            .lock()
            .unwrap()
            .push((from.to_string(), to.to_string()));
        if self.supersede_succeeds {
            *self.armed.lock().unwrap() = None;
            true
        } else {
            false
        }
    }
}

/// A busy host whose tick resolves release `version`, with `0.19.24` installed
/// (i.e. what a pending roll to `v0.19.24` would have left behind).
fn busy_artifact_probe(
    version: &str,
    sha: &str,
    fetch_calls: &Arc<AtomicUsize>,
    rebuild_calls: &Arc<AtomicUsize>,
) -> ArtifactFakeProbe {
    ArtifactFakeProbe {
        artifact: resolved(artifact(version, Some("0.19.24"), Some(sha), Some(SHA_A))),
        check: UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        },
        tree_clean: None,
        // Exactly the shape that makes a roll go pending: sweeps in flight.
        in_flight: 3,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    }
}

/// The issue's headline case: a newer release published while the roll to the
/// previous one is still pending must SUPERSEDE it — the host stops waiting out
/// its paused-dispatch budget for a binary that is already stale, and re-arms
/// for the new one instead.
#[test]
fn test_run_tick_supersedes_a_pending_roll_when_a_newer_artifact_resolves() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let trigger = ArmedTrigger::pending(Some("v0.19.24@aaaa"));
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(
        trigger.supersedes.lock().unwrap().as_slice(),
        &[("v0.19.24@aaaa".to_string(), format!("v0.19.30@{SHA_B}"))],
        "the stale pending roll must be discarded, once"
    );
    assert_eq!(
        fetch_calls.load(Ordering::SeqCst),
        1,
        "the tick must go on to fetch the newer artifact"
    );
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "still no cargo build");
    assert_eq!(
        trigger.targets.lock().unwrap().as_slice(),
        &[Some(format!("v0.19.30@{SHA_B}"))],
        "the replacement roll must be labelled with the artifact it rolls to"
    );
}

/// Superseding is not a licence to churn: a pending roll that already targets
/// the artifact this tick resolved is left completely alone.
#[test]
fn test_run_tick_does_not_supersede_a_pending_roll_for_the_same_artifact() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let trigger = ArmedTrigger::pending(Some(&format!("v0.19.30@{SHA_B}")));
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert!(trigger.supersedes.lock().unwrap().is_empty(), "no supersede");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "no redundant fetch");
    assert!(trigger.targets.lock().unwrap().is_empty(), "no redundant trigger");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("already targets"), "note: {note}");
}

/// A FIRST-ATTEMPT drain is still inside the deadline it was given, so a newer
/// artifact does not yank it out from under the supervisor — #6007's fail-safe
/// window is preserved exactly.
#[test]
fn test_run_tick_never_supersedes_a_first_attempt_drain() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let trigger = ArmedTrigger::new(ArmedRoll {
        target: Some("v0.19.24@aaaa".to_string()),
        pending: false,
        then_exit: false,
        refusals: 0,
    });
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert!(trigger.supersedes.lock().unwrap().is_empty(), "no supersede");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "no fetch");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("first attempt"), "note: {note}");
}

/// A `fleet drain` teardown (`then_exit`) is an operator tearing the host down.
/// It is never superseded by a newer binary — the reverse direction #4521
/// deliberately refuses.
#[test]
fn test_run_tick_never_supersedes_a_teardown_drain() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let trigger = ArmedTrigger::new(ArmedRoll {
        target: Some("v0.19.24@aaaa".to_string()),
        pending: true,
        then_exit: true,
        refusals: 1,
    });
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert!(trigger.supersedes.lock().unwrap().is_empty(), "no supersede");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "no fetch");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("teardown"), "note: {note}");
}

/// An operator's own `restart --drain` carries no artifact target, so there is
/// nothing to compare and nothing to supersede — the daemon must not cancel a
/// drain a human is waiting on.
#[test]
fn test_run_tick_never_supersedes_an_untargeted_operator_drain() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let trigger = ArmedTrigger::pending(None);
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert!(trigger.supersedes.lock().unwrap().is_empty(), "no supersede");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "no fetch");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("no artifact target"), "note: {note}");
}

/// The race: the pending roll completes or is abandoned between the tick's read
/// of the armed state and its supersede call. Discarding nothing must not wedge
/// the tick — it falls through and decides normally, which is exactly what a
/// tick with no armed roll would have done.
#[test]
fn test_run_tick_continues_normally_when_the_roll_ended_before_it_was_superseded() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = busy_artifact_probe("0.19.30", SHA_B, &fetch_calls, &rebuild_calls);
    let mut trigger = ArmedTrigger::pending(Some("v0.19.24@aaaa"));
    trigger.supersede_succeeds = false;
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(trigger.supersedes.lock().unwrap().len(), 1, "supersede attempted");
    assert_eq!(
        fetch_calls.load(Ordering::SeqCst),
        1,
        "the tick still converges on the newer artifact"
    );
    assert_eq!(trigger.targets.lock().unwrap().as_slice(), &[Some(format!("v0.19.30@{SHA_B}"))]);
}
