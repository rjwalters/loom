//! Issue #10712: floor-driven roll targets, end to end through `decide` and
//! `run_tick`.
//!
//! The decision table is floor {unset, satisfied, below, unsatisfiable} x
//! settle {0, 600} x supersede {no, yes}. Only a floor-driven roll skips
//! settle, and every roll is pinned to the exact tag the verdict compared. The
//! no-floor regression proof is that a satisfied or unset floor gives the
//! decisions an untouched state gives, tick for tick. `select_target` and the
//! verdict themselves are unit-tested in `auto_update/floor_roll.rs`.
//!
//! A sibling module rather than more lines in `tests.rs`, per
//! `.loom/docs/file-size-policy.md`.

use super::*;
use crate::auto_update::roll_window::{RollWindowTuning, WindowGate};
use crate::auto_update::supersede::ArmedRoll;
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;
use std::sync::Mutex;

/// The running version in every case below.
const RUNNING: &str = "0.19.800";
/// The newest release every case resolves.
const NEWEST: &str = "0.19.900";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Floor {
    Unset,
    Satisfied,
    Below,
    Unsatisfiable,
}

impl Floor {
    const ALL: [Self; 4] = [
        Self::Unset,
        Self::Satisfied,
        Self::Below,
        Self::Unsatisfiable,
    ];

    fn value(self) -> Option<String> {
        match self {
            Self::Unset => None,
            Self::Satisfied => Some("0.19.700".to_string()),
            // The newest release (0.19.900) satisfies it.
            Self::Below => Some("0.19.850".to_string()),
            // A typo: above every release.
            Self::Unsatisfiable => Some("0.19.9000".to_string()),
        }
    }
}

fn floored(dir: &Path, floor: Floor) -> AutoUpdateState {
    let mut state = state_with_record_dir(dir);
    state.floor.set_basis(floor.value(), RUNNING);
    state
}

fn newest(installed: &str) -> ArtifactResolution {
    resolved(artifact(NEWEST, Some(installed), Some(SHA_B), Some(SHA_A)))
}

fn no_source() -> UpdateCheck {
    UpdateCheck {
        update_available: None,
        source_commit: None,
        commits_behind: None,
        hours_behind: None,
    }
}

fn probe(artifact: ArtifactResolution, fetch_calls: &Arc<AtomicUsize>) -> ArtifactFakeProbe {
    ArtifactFakeProbe {
        artifact,
        check: no_source(),
        tree_clean: None,
        in_flight: 2,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: Arc::new(AtomicUsize::new(0)),
    }
}

/// Optionally armed with a pending roll to an older release (so a newer one
/// supersedes it); records every `trigger_for` target and supersede.
struct Trigger {
    armed: Mutex<Option<ArmedRoll>>,
    targets: Mutex<Vec<Option<String>>>,
    supersedes: Mutex<Vec<(String, String)>>,
}

impl Trigger {
    fn new(pending: bool) -> Self {
        let armed = pending.then(|| ArmedRoll {
            target: Some("v0.19.850@cccc".to_string()),
            pending: true,
            then_exit: false,
            refusals: 1,
        });
        Self {
            armed: Mutex::new(armed),
            targets: Mutex::new(Vec::new()),
            supersedes: Mutex::new(Vec::new()),
        }
    }
}

impl DrainTrigger for Trigger {
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
        self.armed.lock().unwrap().take().is_some()
    }
}

#[test]
fn decision_table_floor_x_settle_x_supersede() {
    for floor in Floor::ALL {
        for settle_secs in [0, 600] {
            for supersede in [false, true] {
                let case = format!("floor={floor:?} settle={settle_secs} supersede={supersede}");
                let tmp = tempfile::tempdir().unwrap();
                let mut state = floored(tmp.path(), floor);
                let fetch_calls = Arc::new(AtomicUsize::new(0));
                let mut probe = probe(newest(RUNNING), &fetch_calls);
                let trigger = Trigger::new(supersede);
                let status = AutoUpdateStatus::new(true);
                let settle = Duration::from_secs(settle_secs);

                let summary = run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);

                // Supersede is decided before the floor is consulted, so it
                // happens (once) whatever the floor says.
                assert_eq!(
                    trigger.supersedes.lock().unwrap().len(),
                    usize::from(supersede),
                    "{case}"
                );
                // Settle is skipped only for a floor-driven roll; a supersede of
                // one does not restart a settle wait.
                let rolls = settle_secs == 0 || floor == Floor::Below;
                assert_eq!(fetch_calls.load(Ordering::SeqCst), usize::from(rolls), "{case}");
                let pinned = format!("v{NEWEST}@{SHA_B}");
                let expected: Vec<Option<String>> = if rolls { vec![Some(pinned)] } else { vec![] };
                assert_eq!(*trigger.targets.lock().unwrap(), expected, "{case}");
                let kind = if rolls {
                    TickDecisionKind::Fetch
                } else {
                    TickDecisionKind::Defer
                };
                assert_eq!(summary.decision, kind, "{case}");
                let note = status.snapshot().note.unwrap_or_default();
                assert_eq!(note.contains("floor-driven"), floor == Floor::Below, "{case}: {note}");
                // Only an unsatisfiable floor stalls, and it never holds a roll back.
                assert_eq!(summary.floor_stall.is_some(), floor == Floor::Unsatisfiable, "{case}");
                assert_eq!(
                    note.contains("FLEET FLOOR UNSATISFIABLE"),
                    floor == Floor::Unsatisfiable,
                    "{case}: {note}"
                );
            }
        }
    }
}

#[test]
fn below_the_floor_installs_the_exact_tag_regardless_of_settle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Below);
    let art = newest(RUNNING);
    let check = no_source();
    let inputs = TickInputs {
        artifact: &art,
        check: &check,
        tree_clean: false,
        in_flight: 0,
    };
    let week = Duration::from_secs(7 * 86_400);
    match state.decide(Instant::now(), &inputs, week, DEFER) {
        TickDecision::FetchArtifact {
            version, tag, why, ..
        } => {
            assert_eq!((version.as_str(), tag.as_str()), (NEWEST, "v0.19.900"));
            assert!(why.contains("below the fleet floor 0.19.850"), "{why}");
            assert!(why.contains("exact tag v0.19.900"), "{why}");
        }
        other => panic!("expected a floor-driven fetch, got {other:?}"),
    }
}

/// A floor-driven roll that goes pending is superseded by a newer release, and
/// the re-armed roll goes out on the same tick under a 600s settle.
#[test]
fn a_supersede_of_a_floor_driven_roll_does_not_re_wait_settle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Below);
    let settle = Duration::from_secs(600);
    let status = AutoUpdateStatus::new(true);
    let first = Arc::new(AtomicUsize::new(0));
    let trigger = Trigger::new(false);
    run_tick(
        &mut state,
        &status,
        &mut probe(newest(RUNNING), &first),
        &trigger,
        settle,
        DEFER,
    );
    assert_eq!(first.load(Ordering::SeqCst), 1, "the floor roll goes out at once");
    // The drain for it times out and the roll goes pending.
    let target = trigger.targets.lock().unwrap()[0].clone();
    *trigger.armed.lock().unwrap() = Some(ArmedRoll {
        target,
        pending: true,
        then_exit: false,
        refusals: 1,
    });
    // A newer release lands; the host still runs 0.19.800, below the floor.
    let second = Arc::new(AtomicUsize::new(0));
    let newer = resolved(artifact("0.19.901", Some(RUNNING), Some(SHA_A), Some(SHA_A)));
    run_tick(&mut state, &status, &mut probe(newer, &second), &trigger, settle, DEFER);
    assert_eq!(trigger.supersedes.lock().unwrap().len(), 1);
    assert_eq!(second.load(Ordering::SeqCst), 1, "no settle wait after the supersede");
    assert_eq!(
        trigger.targets.lock().unwrap().last().cloned().flatten(),
        Some(format!("v0.19.901@{SHA_A}"))
    );
}

/// The no-floor regression proof: across artifact shapes, settle windows, busy
/// and idle hosts, and a sequence of ticks that walks through the settle
/// window and its ceiling, an unset or satisfied floor decides exactly what an
/// untouched state decides.
#[test]
fn an_unset_or_satisfied_floor_decides_exactly_as_before() {
    let shapes = [
        resolved(artifact("0.19.900", Some(RUNNING), Some(SHA_B), Some(SHA_A))), // Newer
        resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_B), Some(SHA_A))),    // ShaDiffers
        resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_A), Some(SHA_A))),    // UpToDate
        resolved(artifact("0.19.700", Some(RUNNING), Some(SHA_A), Some(SHA_A))), // StaleRepo
        unresolved(),
    ];
    let ticks = [0_u64, 300, 700, 1_000, 3_700];
    for basis in [Floor::Unset, Floor::Satisfied] {
        for (i, shape) in shapes.iter().enumerate() {
            for settle_secs in [0_u64, 600] {
                for in_flight in [0_usize, 3] {
                    let tmp = tempfile::tempdir().unwrap();
                    let mut before = state_with_record_dir(tmp.path());
                    let mut after = floored(tmp.path(), basis);
                    let t0 = Instant::now();
                    for (n, at) in ticks.iter().enumerate() {
                        // Alternate the source commit so the settle timer restarts
                        // and the ceiling is what eventually lets it through.
                        let check = stale(&format!("c{n}"));
                        let inputs = TickInputs {
                            artifact: shape,
                            check: &check,
                            tree_clean: true,
                            in_flight,
                        };
                        let now = t0 + Duration::from_secs(*at);
                        let settle = Duration::from_secs(settle_secs);
                        assert_eq!(
                            after.decide(now, &inputs, settle, DEFER),
                            before.decide(now, &inputs, settle, DEFER),
                            "basis={basis:?} shape={i} settle={settle_secs} in_flight={in_flight} t={at}"
                        );
                    }
                    assert!(after.floor.stall().is_none());
                }
            }
        }
    }
}

/// An autoUpdate-only roll (floor satisfied) still honours the roll window.
#[test]
fn an_autoupdate_roll_still_honours_the_roll_window() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Satisfied);
    // A window that opens twelve hours from now.
    let offset = (Utc::now().timestamp() + 43_200).rem_euclid(86_400) as u64;
    state.window = WindowGate::new(RollWindowTuning {
        period: Some(Duration::from_secs(86_400)),
        offset: Duration::from_secs(offset),
        open_for: Duration::from_secs(1800),
        launchd_live_reload: false,
    });
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let status = AutoUpdateStatus::new(true);
    let trigger = Trigger::new(false);
    let mut probe = probe(newest(RUNNING), &fetch_calls);
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("scheduled wait"), "{note}");
}

/// An unsatisfiable floor alerts and keeps the work gate open: no drain is
/// armed, dispatch is not paused, and the tick carries the typed stall.
#[tokio::test]
async fn an_unsatisfiable_floor_alerts_and_keeps_dispatching() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    let drain = Arc::new(DrainState::new());
    let trigger =
        IpcDrainTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());
    let mut state = floored(tmp.path(), Floor::Unsatisfiable);
    // The newest release is the running one: nothing for autoUpdate to do.
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let current = resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_A), Some(SHA_A)));
    let mut probe = probe(current, &fetch_calls);
    let status = AutoUpdateStatus::new(true);

    let summary = run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);

    assert!(!drain.is_draining(), "the work gate stays open");
    assert!(!summary.roll_armed);
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(summary.decision, TickDecisionKind::Skip);
    let stall = state.floor.stall().cloned().expect("a typed stall");
    assert_eq!(
        (stall.floor.as_str(), stall.running.as_str(), stall.newest.as_str()),
        ("0.19.9000", RUNNING, RUNNING)
    );
    assert_eq!(summary.floor_stall, Some(stall.note()));
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("DISPATCH CONTINUES"), "{note}");
}
