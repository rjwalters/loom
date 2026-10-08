//! Issue #9132: what `run_tick` does with a configured roll window, end to end.
//!
//! The schedule arithmetic and the gate are unit-tested in
//! `auto_update/roll_window/tests.rs` with an injected clock. `run_tick` reads the
//! wall clock, so these tests place the window relative to *now* (open since a
//! minute ago, or opening twelve hours from now) rather than at a fixed time.

use super::*;
use crate::auto_update::roll_window::{RollWindowTuning, WindowGate};
use crate::auto_update::supersede::ArmedRoll;
use std::sync::Mutex;

const DAY: u64 = 86_400;

/// Arms on `trigger_for` (labelled, not yet pending) exactly like the daemon, and
/// clears on `abandon_roll` like `DrainState::abort()`.
#[derive(Default)]
struct ArmingTrigger {
    armed: Mutex<Option<ArmedRoll>>,
    arms: Mutex<usize>,
    abandons: Mutex<usize>,
}

impl ArmingTrigger {
    fn go_pending(&self) {
        if let Some(roll) = self.armed.lock().unwrap().as_mut() {
            roll.pending = true;
            roll.refusals = 1;
        }
    }
}

impl DrainTrigger for ArmingTrigger {
    fn trigger(&self) -> bool {
        true
    }
    fn trigger_for(&self, target: Option<&str>) -> bool {
        *self.arms.lock().unwrap() += 1;
        *self.armed.lock().unwrap() = Some(ArmedRoll {
            target: target.map(str::to_string),
            pending: false,
            then_exit: false,
            refusals: 0,
        });
        true
    }
    fn roll_in_progress(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
    fn abandon_roll(&self, _reason: &str) -> bool {
        *self.abandons.lock().unwrap() += 1;
        self.armed.lock().unwrap().take().is_some()
    }
}

/// A window that started `started_secs_ago` ago (negative: opens in the future).
fn windowed_state(dir: &Path, started_secs_ago: i64) -> AutoUpdateState {
    let offset = (Utc::now().timestamp() - started_secs_ago).rem_euclid(DAY as i64) as u64;
    let mut state = state_with_record_dir(dir);
    state.window = WindowGate::new(RollWindowTuning {
        period: Some(Duration::from_secs(DAY)),
        offset: Duration::from_secs(offset),
        open_for: Duration::from_secs(1800),
        launchd_live_reload: false,
    });
    state
}

fn probe(fetch_calls: &Arc<AtomicUsize>, rebuild_calls: &Arc<AtomicUsize>) -> ArtifactFakeProbe {
    ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.390", Some("0.19.389"), Some(SHA_B), Some(SHA_A))),
        check: UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        },
        tree_clean: None,
        in_flight: 2,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    }
}

fn tick(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut ArtifactFakeProbe,
    trigger: &ArmingTrigger,
) {
    // A long settle: a windowed loop must ignore it.
    run_tick(state, status, probe, trigger, Duration::from_secs(3600), DEFER);
}

#[test]
fn a_new_build_outside_the_window_fetches_and_arms_nothing_and_status_says_why() {
    let (fetch, rebuild) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut probe = probe(&fetch, &rebuild);
    let trigger = ArmingTrigger::default();
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = windowed_state(tmp.path(), -(DAY as i64 / 2));

    for _ in 0..3 {
        tick(&mut state, &status, &mut probe, &trigger);
    }
    assert_eq!(fetch.load(Ordering::SeqCst), 0);
    assert_eq!(*trigger.arms.lock().unwrap(), 0);
    let snap = status.snapshot();
    assert!(snap.note.unwrap().starts_with("scheduled wait"));
    let window = snap.roll_window.expect("a configured window is published");
    assert!(!window.window_open_now);
    assert_eq!(window.roll_target.as_deref(), Some("0.19.390"));
    assert!(window.deferral.unwrap().starts_with("scheduled wait"));
    assert!(window.next_window_open > Utc::now());
}

#[test]
fn an_open_window_arms_once_then_a_timed_out_drain_is_abandoned_and_not_rearmed() {
    let (fetch, rebuild) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut probe = probe(&fetch, &rebuild);
    let trigger = ArmingTrigger::default();
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = windowed_state(tmp.path(), 60);

    // Settle (3600s) is bypassed: the first in-window tick rolls.
    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(fetch.load(Ordering::SeqCst), 1);
    assert_eq!(*trigger.arms.lock().unwrap(), 1);

    // The drain times out (goes pending): the next tick abandons it so dispatch resumes.
    trigger.go_pending();
    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(*trigger.abandons.lock().unwrap(), 1);
    assert!(trigger.armed_roll().is_none(), "dispatch resumed");
    assert_eq!(fetch.load(Ordering::SeqCst), 1);

    // More ticks in the same (still open) window: no re-arm.
    for _ in 0..3 {
        tick(&mut state, &status, &mut probe, &trigger);
    }
    assert_eq!(fetch.load(Ordering::SeqCst), 1, "no re-fetch in a spent window");
    assert_eq!(*trigger.arms.lock().unwrap(), 1, "no re-arm in a spent window");
    let note = status.snapshot().note.unwrap();
    assert!(note.starts_with("drain timed out, waiting for next window"), "{note}");
}

#[test]
fn without_a_window_the_loop_is_unchanged() {
    let (fetch, rebuild) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut probe = probe(&fetch, &rebuild);
    let trigger = ArmingTrigger::default();
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    // Settle is honoured when no window is configured (first sighting waits).
    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(fetch.load(Ordering::SeqCst), 0);
    assert!(status.snapshot().roll_window.is_none());
    assert!(status.snapshot().note.unwrap().contains("settle"));
}

/// #10713 / #10188 item 2: the tick that arms a roll writes the window's
/// consumption to disk itself, because the roll's restart can end the process
/// before any later tick would record it.
#[test]
fn the_arming_tick_persists_the_consumed_window_before_the_roll_can_restart() {
    use crate::auto_update::persisted_state::{load, LoadOutcome, STATE_FILE};
    let (fetch, rebuild) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut probe = probe(&fetch, &rebuild);
    let trigger = ArmingTrigger::default();
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(STATE_FILE);
    let mut state = windowed_state(tmp.path(), 60);
    state.attach_persistence(Some(path.clone()));

    tick(&mut state, &status, &mut probe, &trigger);
    assert_eq!(*trigger.arms.lock().unwrap(), 1);
    let LoadOutcome::Loaded(saved) = load(&path) else {
        panic!("the arming tick wrote the state file");
    };
    assert!(saved.window.expect("windowing is on").consumed.is_some());

    // The "restarted" daemon: a fresh state on the same schedule, no roll armed.
    let mut restarted = windowed_state(tmp.path(), 60);
    restarted.attach_persistence(Some(path));
    let fresh_trigger = ArmingTrigger::default();
    tick(&mut restarted, &status, &mut probe, &fresh_trigger);
    assert_eq!(
        *fresh_trigger.arms.lock().unwrap(),
        0,
        "one roll per window, across the restart"
    );
    assert_eq!(fetch.load(Ordering::SeqCst), 1);
}
