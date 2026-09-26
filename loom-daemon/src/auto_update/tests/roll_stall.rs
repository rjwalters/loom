//! Issue #8998: what `run_tick` does when the roll it keeps arming can never
//! complete.
//!
//! The pure state machine is unit-tested in `auto_update/roll_stall.rs`. This
//! module pins the end-to-end consequences through `run_tick`, because the
//! defect was never in one component — it was in the *sequence*: arm → refuse →
//! retain → abandon → arm again, with dispatch (and role spawns) paused for most
//! of it, for 21 hours.
//!
//! A sibling module rather than more lines in `tests.rs`, per
//! `.loom/docs/file-size-policy.md` (that file is over the ratchet threshold).

use super::*;
use crate::auto_update::roll_stall::{
    resolve_roll_stall_deadlines, AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV,
    DEFAULT_ROLL_STALL_DEADLINES,
};
use crate::auto_update::supersede::ArmedRoll;
use std::sync::Mutex;

/// A trigger standing in for a daemon with a roll armed, whose refusal count an
/// individual test advances tick by tick. It records every `abandon_roll` and
/// every `trigger_for`, and — when abandoned — flips to "no roll armed" exactly
/// as `DrainState::abort()` does.
struct StallingTrigger {
    armed: Mutex<Option<ArmedRoll>>,
    abandons: Arc<Mutex<Vec<String>>>,
    targets: Arc<Mutex<Vec<Option<String>>>>,
}

impl StallingTrigger {
    fn new(armed: Option<ArmedRoll>) -> Self {
        Self {
            armed: Mutex::new(armed),
            abandons: Arc::new(Mutex::new(Vec::new())),
            targets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A pending auto-update roll that has refused `refusals` deadlines. Its
    /// target is deliberately the SAME artifact `stuck_host_probe` resolves, so
    /// #8514's supersede path stays out of the way and the only thing under test
    /// is #8998's unsatisfiability verdict.
    fn pending(refusals: u32) -> Self {
        Self::new(Some(ArmedRoll {
            target: Some(format!("v0.19.390@{SHA_B}")),
            pending: refusals > 0,
            then_exit: false,
            refusals,
        }))
    }

    fn set_refusals(&self, refusals: u32) {
        if let Some(roll) = self.armed.lock().unwrap().as_mut() {
            roll.refusals = refusals;
            roll.pending = refusals > 0;
        }
    }

    fn abandon_count(&self) -> usize {
        self.abandons.lock().unwrap().len()
    }
}

impl DrainTrigger for StallingTrigger {
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
    fn abandon_roll(&self, reason: &str) -> bool {
        self.abandons.lock().unwrap().push(reason.to_string());
        // Mirrors the production path: `DrainState::abort()` clears the drain, so
        // the next tick sees no armed roll.
        self.armed.lock().unwrap().take().is_some()
    }
}

/// A busy host with `0.19.389` installed and `0.19.390` published — i.e. a host
/// that wants to roll and has sweeps in flight that will not finish.
fn stuck_host_probe(
    fetch_calls: &Arc<AtomicUsize>,
    rebuild_calls: &Arc<AtomicUsize>,
) -> ArtifactFakeProbe {
    ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.390", Some("0.19.389"), Some(SHA_B), Some(SHA_A))),
        check: UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        },
        tree_clean: None,
        // The 9h32m analog-simulation sweep from the incident, plus friends.
        in_flight: 3,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    }
}

fn tick(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut ArtifactFakeProbe,
    trigger: &StallingTrigger,
) {
    run_tick(state, status, probe, trigger, Duration::from_secs(0), DEFER);
}

/// The headline case. Three drain deadlines expire with the in-flight count
/// never improving, so the roll is ABANDONED rather than re-armed a fourth
/// time — dispatch (and with it role spawns) resumes instead of staying paused
/// indefinitely.
#[test]
fn test_run_tick_abandons_a_roll_whose_drain_condition_is_unsatisfiable() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::pending(1);
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    // Deadline 1 and 2: the #6007 retention is still doing its job, untouched.
    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(trigger.abandon_count(), 0, "a first refusal must be left alone");
    trigger.set_refusals(2);
    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(trigger.abandon_count(), 0, "a second refusal must be left alone");

    // Deadline 3: no improvement on 3 in flight across any of them.
    trigger.set_refusals(3);
    tick(&mut state, &status, &mut probe, &trigger);

    assert_eq!(trigger.abandon_count(), 1, "the roll must be abandoned exactly once");
    let reason = trigger.abandons.lock().unwrap()[0].clone();
    assert!(reason.contains("UNSATISFIABLE"), "{reason}");
    assert!(reason.contains("3 drain deadline(s)"), "{reason}");
    assert!(reason.contains("NORMAL DISPATCH RESUMES"), "{reason}");
    assert!(reason.contains("No sweep was cancelled"), "{reason}");
    assert!(reason.contains("v0.19.390@"), "the note must name the roll: {reason}");
    assert!(trigger.armed_roll().is_none(), "dispatch resumes — nothing stays armed");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "abandoning is not a roll");
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "and never a rebuild");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(
        note.contains("UNSATISFIABLE"),
        "`status --json`'s auto_update_note must carry it: {note}"
    );
}

/// The re-arm is what made this a livelock rather than a bounded failure: a
/// newer release resolving on the very next tick must NOT arm another roll while
/// the host still cannot drain.
#[test]
fn test_run_tick_does_not_re_arm_while_the_condition_stays_unsatisfiable() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::pending(3);
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(trigger.abandon_count(), 1);

    // Four more ticks with nothing armed and a newer artifact still resolving —
    // the fleet cuts a release every ~30 min, which is exactly what kept
    // re-arming the roll.
    for _ in 0..4 {
        tick(&mut state, &status, &mut probe, &trigger);
    }
    assert!(trigger.targets.lock().unwrap().is_empty(), "no roll may be armed");
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "and no fetch either");
    assert_eq!(trigger.abandon_count(), 1, "nothing to abandon a second time");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(
        note.contains("UNSATISFIABLE"),
        "the state is re-reported, not logged once: {note}"
    );
}

/// The suppression is self-clearing: the moment in-flight reaches zero the
/// condition is demonstrably satisfiable, so the roll arms and completes on its
/// own with no operator action.
#[test]
fn test_an_idle_observation_clears_the_suppression_and_the_roll_arms_normally() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::pending(3);
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(trigger.abandon_count(), 1);

    // The long sweep finishes.
    probe.in_flight = 0;
    tick(&mut state, &status, &mut probe, &trigger);

    assert_eq!(fetch_calls.load(Ordering::SeqCst), 1, "the host must roll once it can");
    assert_eq!(
        trigger.targets.lock().unwrap().as_slice(),
        &[Some(format!("v0.19.390@{SHA_B}"))],
        "and the replacement roll is labelled with the artifact it rolls to"
    );
}

/// `fleet drain`'s teardown is an operator tearing the host down. It is never
/// abandoned, however long it waits — the same rule #8514 applies to superseding.
#[test]
fn test_run_tick_never_abandons_a_teardown_drain() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::new(Some(ArmedRoll {
        target: Some("v0.19.390@aaaa".to_string()),
        pending: true,
        then_exit: true,
        refusals: 9,
    }));
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    for _ in 0..5 {
        tick(&mut state, &status, &mut probe, &trigger);
    }
    assert_eq!(trigger.abandon_count(), 0);
    assert!(trigger.armed_roll().is_some(), "the teardown stays armed");
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("teardown"), "the pre-#8998 skip wording is preserved: {note}");
}

/// An operator `restart --drain` carries no artifact target. The auto-updater
/// must not countermand it — the daemon abandons its OWN rolls, never someone
/// else's drain.
#[test]
fn test_run_tick_never_abandons_an_untargeted_operator_drain() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::new(Some(ArmedRoll {
        target: None,
        pending: true,
        then_exit: false,
        refusals: 9,
    }));
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    for _ in 0..5 {
        tick(&mut state, &status, &mut probe, &trigger);
    }
    assert_eq!(trigger.abandon_count(), 0);
    assert!(trigger.armed_roll().is_some(), "the operator's drain stays armed");
}

/// The threshold is a knob, not a constant baked into the tick.
#[test]
fn test_the_deadline_threshold_is_configurable() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = stuck_host_probe(&fetch_calls, &rebuild_calls);
    let trigger = StallingTrigger::pending(1);
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path()).with_roll_stall_deadlines(1);

    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(trigger.abandon_count(), 1, "one deadline is enough at threshold 1");
}

#[test]
#[serial(loom_auto_update_env)]
fn test_the_threshold_resolves_env_over_config_over_default() {
    let configured = AutoUpdateConfig {
        roll_stall_deadlines: Some(7),
        ..AutoUpdateConfig::default()
    };
    std::env::remove_var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV);
    assert_eq!(resolve_roll_stall_deadlines(&configured), 7);
    assert_eq!(
        resolve_roll_stall_deadlines(&AutoUpdateConfig::default()),
        DEFAULT_ROLL_STALL_DEADLINES
    );

    std::env::set_var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV, "2");
    assert_eq!(resolve_roll_stall_deadlines(&configured), 2, "env wins");
    // A zero/unparseable env value falls through rather than disabling the
    // detector (a `0` threshold would declare every armed roll unsatisfiable).
    std::env::set_var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV, "0");
    assert_eq!(resolve_roll_stall_deadlines(&configured), 7);
    std::env::set_var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV, "not-a-number");
    assert_eq!(resolve_roll_stall_deadlines(&configured), 7);
    std::env::remove_var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV);
}

/// The production abandonment goes through the operator `--abort-drain`
/// primitive, so it clears the pause flag, bumps the generation (the live
/// supervisor stands down without exiting the process) and resets #6007's whole
/// pending-roll bookkeeping — and the note it leaves behind in `status --json`'s
/// `drain_note` says the daemon gave up, not that an operator did.
#[tokio::test]
async fn test_ipc_drain_trigger_abandonment_resumes_dispatch_and_renames_the_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    let drain = Arc::new(DrainState::new());
    let trigger =
        IpcDrainTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());

    // Arm a relaunch roll directly and drive it to "pending" the way a refused
    // deadline would, so the state being abandoned is the real one.
    drain.begin(Duration::from_secs(1800), false, false);
    drain.set_roll_target(Some("v0.19.390@aaaa".to_string()));
    let refusal = drain.refuse_roll_deadline(chrono::Utc::now() + chrono::Duration::seconds(1800));
    assert!(matches!(refusal, crate::ipc::RollRefusal::Deferred { .. }), "{refusal:?}");
    assert!(drain.is_draining(), "dispatch is paused — #6007's retention");
    let generation = drain.generation();

    assert!(trigger.abandon_roll("unsatisfiable: giving up on the roll"));

    assert!(!drain.is_draining(), "dispatch must resume, including role spawns");
    assert!(drain.generation() > generation, "the live supervisor must stand down");
    let snap = drain.snapshot();
    assert!(!snap.active);
    assert!(!snap.roll_pending, "no latched pending roll");
    assert_eq!(snap.roll_target, None);
    assert_eq!(snap.deadline, None);
    assert_eq!(snap.note.as_deref(), Some("unsatisfiable: giving up on the roll"));

    // Idempotent: nothing left to abandon.
    assert!(!trigger.abandon_roll("again"));
}
