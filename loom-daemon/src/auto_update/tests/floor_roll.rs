//! Issues #10712 and #10885: the fleet floor end to end through `decide` and
//! `run_tick`.
//!
//! The decision table is floor {no store, unknown, satisfied, below,
//! unsatisfiable} x settle {0, 600} x supersede {no, yes}. A host with a fleet
//! store rolls only when it is below a floor a release can meet, on that tick
//! and whatever `settleSecs` says; in every other row it does not roll, and a
//! newer release, a re-published artifact or a newer source HEAD is not
//! chased. A host with no fleet store keeps settle-gated autoUpdate.
//! `select_target` and the verdict themselves are unit-tested in
//! `auto_update/floor_roll.rs`.
//!
//! A sibling module rather than more lines in `tests.rs`, per
//! `.loom/docs/file-size-policy.md`.

use super::*;
use crate::auto_update::supersede::ArmedRoll;
use crate::auto_update::tick_telemetry::TickSummary;
use crate::fleet_sync::FloorKnowledge;
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;
use std::sync::Mutex;

/// The running version in every case below.
const RUNNING: &str = "0.19.800";
/// The newest release every case resolves.
const NEWEST: &str = "0.19.900";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Floor {
    /// No fleet store: not a fleet host.
    NoStore,
    /// A fleet store, but no floor known.
    Unknown,
    Satisfied,
    Below,
    Unsatisfiable,
}

impl Floor {
    const ALL: [Self; 5] = [
        Self::NoStore,
        Self::Unknown,
        Self::Satisfied,
        Self::Below,
        Self::Unsatisfiable,
    ];

    fn knowledge(self) -> FloorKnowledge {
        let set = |v: &str| FloorKnowledge::Set(v.to_string());
        match self {
            Self::NoStore => FloorKnowledge::NoStore,
            Self::Unknown => FloorKnowledge::Unknown("no pass has completed".to_string()),
            Self::Satisfied => set("0.19.700"),
            // The newest release (0.19.900) satisfies it.
            Self::Below => set("0.19.850"),
            // A typo: above every release.
            Self::Unsatisfiable => set("0.19.9000"),
        }
    }
}

fn floored(dir: &Path, floor: Floor) -> AutoUpdateState {
    let mut state = state_with_record_dir(dir);
    state.floor.set_basis(floor.knowledge(), RUNNING);
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
/// supersedes it); records every `trigger_pause_roll` target, its
/// `target_source`, and every supersede.
struct Trigger {
    armed: Mutex<Option<ArmedRoll>>,
    targets: Mutex<Vec<Option<String>>>,
    sources: Mutex<Vec<pause_manifest::TargetSource>>,
    supersedes: Mutex<Vec<(String, String)>>,
}

impl Trigger {
    fn new(pending: bool) -> Self {
        let armed = pending.then(|| ArmedRoll {
            target: Some("v0.19.850@cccc".to_string()),
            committed: false,
            then_exit: false,
        });
        Self {
            armed: Mutex::new(armed),
            targets: Mutex::new(Vec::new()),
            sources: Mutex::new(Vec::new()),
            supersedes: Mutex::new(Vec::new()),
        }
    }
}

impl RollTrigger for Trigger {
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool {
        self.targets.lock().unwrap().push(target.label.clone());
        self.sources.lock().unwrap().push(target.source.clone());
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
                // A fleet host rolls only below a floor a release meets, and
                // then whatever settle says (a supersede of that roll does not
                // restart a settle wait). Only a host with no fleet store
                // chases the newest release, behind settle.
                let rolls = floor == Floor::Below || (floor == Floor::NoStore && settle_secs == 0);
                assert_eq!(fetch_calls.load(Ordering::SeqCst), usize::from(rolls), "{case}");
                let pinned = format!("v{NEWEST}@{SHA_B}");
                let expected: Vec<Option<String>> = if rolls { vec![Some(pinned)] } else { vec![] };
                assert_eq!(*trigger.targets.lock().unwrap(), expected, "{case}");
                // #10831: floor-driven and autoUpdate rolls take the same pause
                // trigger; only the recorded `target_source` differs.
                let source = if floor == Floor::Below {
                    pause_manifest::TargetSource::Floor
                } else {
                    pause_manifest::TargetSource::AutoUpdate
                };
                let sources: Vec<_> = if rolls { vec![source] } else { vec![] };
                assert_eq!(*trigger.sources.lock().unwrap(), sources, "{case}");
                let kind = match (rolls, floor) {
                    (true, _) => TickDecisionKind::Fetch,
                    // Held by the settle gate, with a target still tracked.
                    (false, Floor::NoStore) => TickDecisionKind::Defer,
                    // #10885: a fleet host that is not rolling tracks nothing.
                    (false, _) => TickDecisionKind::Skip,
                };
                assert_eq!(summary.decision, kind, "{case}");
                let note = status.snapshot().note.unwrap_or_default();
                assert_eq!(note.contains("floor-driven"), floor == Floor::Below, "{case}: {note}");
                // Only an unsatisfiable floor stalls. #10885: it no longer
                // falls back to the newest release (0.19.900 > running here).
                assert_eq!(summary.floor_stall.is_some(), floor == Floor::Unsatisfiable, "{case}");
                assert_eq!(
                    note.contains("fleet floor not known"),
                    floor == Floor::Unknown,
                    "{case}: {note}"
                );
                assert_eq!(
                    note.contains("fleet floor 0.19.700 is met"),
                    floor == Floor::Satisfied,
                    "{case}: {note}"
                );
                if floor != Floor::NoStore && !rolls {
                    assert!(state.tracked_target.is_none(), "{case}: no settle clock");
                    assert!(state.first_stale_since.is_none(), "{case}: no settle ceiling");
                }
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
    // Its pause is armed and has not stopped anything yet.
    let target = trigger.targets.lock().unwrap()[0].clone();
    *trigger.armed.lock().unwrap() = Some(ArmedRoll {
        target,
        committed: false,
        then_exit: false,
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

/// One `decide` against `artifact` at `at` seconds, with a clean tree and a
/// source checkout that is `source_ahead` of the running binary.
fn decide_at(
    state: &mut AutoUpdateState,
    t0: Instant,
    at: u64,
    artifact: &ArtifactResolution,
    source_ahead: bool,
    settle: Duration,
) -> TickDecision {
    let check = if source_ahead {
        stale(&format!("c{at}"))
    } else {
        no_source()
    };
    let inputs = TickInputs {
        artifact,
        check: &check,
        tree_clean: true,
        in_flight: 0,
    };
    state.decide(t0 + Duration::from_secs(at), &inputs, settle, DEFER)
}

/// A same-version artifact whose published sha differs from the installed one.
fn republished() -> ArtifactResolution {
    resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_B), Some(SHA_A)))
}

/// #10885: with the floor met, nothing is chased. Not a newer release, not a
/// re-published artifact, not a newer source HEAD, and not after the settle
/// ceiling has elapsed, because no settle clock runs at all.
#[test]
fn a_satisfied_floor_never_chases_a_newer_release_a_republish_or_source_head() {
    let settle = Duration::from_secs(600);
    let shapes: [(&str, ArtifactResolution, bool); 3] = [
        ("newer release", newest(RUNNING), false),
        ("re-published artifact", republished(), false),
        ("source HEAD ahead", unresolved(), true),
    ];
    for (shape, artifact, source_ahead) in &shapes {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = floored(tmp.path(), Floor::Satisfied);
        let t0 = Instant::now();
        // Past the quiet period (600s) and the ceiling (3600s).
        for at in [0_u64, 300, 700, 3_700, 7_300] {
            let decision = decide_at(&mut state, t0, at, artifact, *source_ahead, settle);
            let TickDecision::Skip(reason) = decision else {
                panic!("{shape} t={at}: expected no roll, got {decision:?}");
            };
            assert!(reason.contains("fleet floor 0.19.700 is met"), "{shape}: {reason}");
            assert!(reason.contains("not chasing"), "{shape}: {reason}");
            assert!(state.tracked_target.is_none(), "{shape} t={at}");
            assert!(state.stale_since.is_none(), "{shape} t={at}");
            assert!(state.first_stale_since.is_none(), "{shape} t={at}");
        }
    }

    // Through the tick: nothing fetched, nothing rebuilt, nothing armed, and
    // the note names the floor and the release that was not chased.
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Satisfied);
    let (fetches, rebuilds) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let trigger = Trigger::new(false);
    let status = AutoUpdateStatus::new(true);
    for (_, artifact, source_ahead) in &shapes {
        let mut probe = probe(artifact.clone(), &fetches);
        probe.rebuild_calls = rebuilds.clone();
        probe.tree_clean = Some(true);
        probe.in_flight = 0;
        if *source_ahead {
            probe.check = stale("c1");
        }
        let summary = run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
        assert_eq!(summary.decision, TickDecisionKind::Skip, "{}", summary.note);
        assert!(!summary.roll_armed);
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    assert_eq!(rebuilds.load(Ordering::SeqCst), 0);
    assert!(trigger.targets.lock().unwrap().is_empty());
    let mut probe = probe(newest(RUNNING), &fetches);
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("fleet floor 0.19.700 is met by running 0.19.800"), "{note}");
    assert!(note.contains("not chasing release v0.19.900"), "{note}");
    assert!(!note.contains("fetching"), "{note}");
}

/// #10885: a floor no release meets raises the stall and does not roll to the
/// newest release instead, however long it stands.
#[test]
fn an_unsatisfiable_floor_does_not_fall_back_to_the_newest_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Unsatisfiable);
    let settle = Duration::from_secs(600);
    let t0 = Instant::now();
    // 0.19.900 is newer than the running 0.19.800 but below the floor.
    for at in [0_u64, 700, 3_700, 7_300] {
        let decision = decide_at(&mut state, t0, at, &newest(RUNNING), false, settle);
        assert!(matches!(decision, TickDecision::Skip(_)), "t={at}: {decision:?}");
        let stall = state.floor.stall().expect("the typed stall stands");
        assert_eq!(stall.newest, NEWEST);
        assert!(state.first_stale_since.is_none(), "t={at}");
    }
}

/// #10885: a fleet host whose floor is not known does nothing and says so.
/// It never falls back to chasing the latest release or rebuilding.
#[test]
fn an_unknown_floor_rolls_nothing_and_says_the_floor_is_not_known() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Unknown);
    let (fetches, rebuilds) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let trigger = Trigger::new(false);
    let status = AutoUpdateStatus::new(true);
    for (artifact, source_ahead) in [(newest(RUNNING), false), (unresolved(), true)] {
        let mut probe = probe(artifact, &fetches);
        probe.rebuild_calls = rebuilds.clone();
        probe.tree_clean = Some(true);
        probe.in_flight = 0;
        if source_ahead {
            probe.check = stale("c1");
        }
        let summary = run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
        assert_eq!(summary.decision, TickDecisionKind::Skip, "{}", summary.note);
        assert!(!summary.roll_armed);
        assert_eq!(summary.floor_stall, None);
        let note = status.snapshot().note.unwrap_or_default();
        assert!(note.contains("fleet floor not known (no pass has completed)"), "{note}");
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    assert_eq!(rebuilds.load(Ordering::SeqCst), 0);
    assert!(trigger.targets.lock().unwrap().is_empty());
}

/// #10885: below the floor with no release resolved (the newest tag has no
/// assets yet, or the forge is unreachable), the host waits for the next tick.
/// The source-rebuild path is not a substitute.
#[test]
fn below_the_floor_with_no_release_resolved_waits_and_never_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Below);
    let (fetches, rebuilds) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut probe = probe(unresolved(), &fetches);
    probe.rebuild_calls = rebuilds.clone();
    probe.tree_clean = Some(true);
    probe.in_flight = 0;
    probe.check = stale("c1");
    let status = AutoUpdateStatus::new(true);
    let trigger = Trigger::new(false);
    let summary = run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
    assert_eq!(summary.decision, TickDecisionKind::Skip, "{}", summary.note);
    assert_eq!(rebuilds.load(Ordering::SeqCst), 0);
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    let note = status.snapshot().note.unwrap_or_default();
    assert!(note.contains("no release resolved this tick"), "{note}");

    // The release appears: the next tick rolls to it.
    let mut probe = self::probe(newest(RUNNING), &fetches);
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(600), DEFER);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

/// A running version that is not `X.Y.Z` cannot be compared with the floor:
/// no version roll (fail closed), where it used to mean "no floor".
#[test]
fn an_uncomparable_running_version_rolls_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());
    state
        .floor
        .set_basis(FloorKnowledge::Set("0.19.850".to_string()), "dev");
    let t0 = Instant::now();
    let decision = decide_at(&mut state, t0, 0, &newest(RUNNING), false, Duration::ZERO);
    let TickDecision::Skip(reason) = decision else {
        panic!("expected no roll, got {decision:?}");
    };
    assert!(reason.contains("cannot be compared"), "{reason}");
}

/// A host with no fleet store is untouched by all of the above: an explicit
/// `NoStore` basis decides exactly what a state that never heard of a floor
/// decides, across artifact shapes, settle windows, busy and idle hosts, and a
/// sequence of ticks that walks through the settle window and its ceiling.
#[test]
fn a_host_with_no_fleet_store_decides_exactly_as_before() {
    let shapes = [
        resolved(artifact("0.19.900", Some(RUNNING), Some(SHA_B), Some(SHA_A))), // Newer
        resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_B), Some(SHA_A))),    // ShaDiffers
        resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_A), Some(SHA_A))),    // UpToDate
        resolved(artifact("0.19.700", Some(RUNNING), Some(SHA_A), Some(SHA_A))), // StaleRepo
        unresolved(),
    ];
    let ticks = [0_u64, 300, 700, 1_000, 3_700];
    let mut rolled = 0;
    for (i, shape) in shapes.iter().enumerate() {
        for settle_secs in [0_u64, 600] {
            for in_flight in [0_usize, 3] {
                let tmp = tempfile::tempdir().unwrap();
                let mut before = state_with_record_dir(tmp.path());
                let mut after = floored(tmp.path(), Floor::NoStore);
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
                    let decided = after.decide(now, &inputs, settle, DEFER);
                    assert_eq!(
                        decided,
                        before.decide(now, &inputs, settle, DEFER),
                        "shape={i} settle={settle_secs} in_flight={in_flight} t={at}"
                    );
                    rolled += usize::from(matches!(
                        decided,
                        TickDecision::FetchArtifact { .. } | TickDecision::Rebuild { .. }
                    ));
                }
                assert!(after.floor.stall().is_none());
            }
        }
    }
    assert!(rolled > 0, "the table exercises rolls, not only skips");
}

/// A store that stops being configured (`fleet.repo` removed, which takes a
/// restart) returns the host to settle-gated autoUpdate without an instant
/// roll: nothing was tracked while the floor held, so the settle clock starts
/// at the tick that first sees no store.
#[test]
fn losing_the_fleet_store_starts_the_settle_clock_from_that_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Satisfied);
    let settle = Duration::from_secs(600);
    let t0 = Instant::now();
    // The newer release has been visible for two hours, never chased.
    for at in [0_u64, 3_600, 7_200] {
        let held = decide_at(&mut state, t0, at, &newest(RUNNING), false, settle);
        assert!(matches!(held, TickDecision::Skip(_)), "t={at}");
    }
    state.floor.set_basis(FloorKnowledge::NoStore, RUNNING);
    let removal = decide_at(&mut state, t0, 7_300, &newest(RUNNING), false, settle);
    let TickDecision::Skip(reason) = removal else {
        panic!("no roll on the removal tick, got {removal:?}");
    };
    assert!(reason.contains("within settle window"), "{reason}");
    let mid = decide_at(&mut state, t0, 7_300 + 599, &newest(RUNNING), false, settle);
    assert!(matches!(mid, TickDecision::Skip(_)), "{mid:?}");
    let settled = decide_at(&mut state, t0, 7_300 + 600, &newest(RUNNING), false, settle);
    assert!(matches!(settled, TickDecision::FetchArtifact { .. }), "{settled:?}");
}

/// #10880 relies on it: the tick that arms a roll writes the state file
/// itself, because the roll's restart can end the process before the
/// end-of-tick save (which `guarded_tick`, not `run_tick`, performs).
#[test]
fn the_arming_tick_persists_state_before_the_roll_can_restart() {
    use crate::auto_update::persisted_state::{load, LoadOutcome, STATE_FILE};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(STATE_FILE);
    let mut state = floored(tmp.path(), Floor::Below);
    state.attach_persistence(Some(path.clone()));
    let fetches = Arc::new(AtomicUsize::new(0));
    let trigger = Trigger::new(false);
    let status = AutoUpdateStatus::new(true);
    let mut probe = probe(newest(RUNNING), &fetches);
    let summary = run_tick(&mut state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
    assert!(summary.roll_armed);
    assert!(matches!(load(&path), LoadOutcome::Loaded(_)), "written before the arm");
}

/// An unsatisfiable floor alerts and keeps the work gate open: no pause is
/// started (through the production trigger), dispatch is not paused, and the
/// tick carries the typed stall.
#[tokio::test]
async fn an_unsatisfiable_floor_alerts_and_keeps_dispatching() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    let drain = Arc::new(DrainState::new());
    let trigger =
        IpcRollTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());
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

/// One tick with settle 0 and a fresh fetch counter; returns the summary and
/// the published note.
fn tick(state: &mut AutoUpdateState, artifact: ArtifactResolution) -> (TickSummary, String) {
    let status = AutoUpdateStatus::new(true);
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = probe(artifact, &fetch_calls);
    let trigger = Trigger::new(false);
    let summary = run_tick(state, &status, &mut probe, &trigger, Duration::ZERO, DEFER);
    (summary, status.snapshot().note.unwrap_or_default())
}

/// The newest release is the running one: nothing for autoUpdate to do.
fn current() -> ArtifactResolution {
    resolved(artifact(RUNNING, Some(RUNNING), Some(SHA_A), Some(SHA_A)))
}

/// #10866 item 1: the ERROR line is logged when the stall starts and not on
/// the next tick, while the stall itself is reported on both.
#[test]
fn a_standing_unsatisfiable_floor_alerts_once_and_is_reported_every_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Unsatisfiable);
    let alerted: Vec<bool> = (0..2)
        .map(|n| {
            let (summary, note) = tick(&mut state, current());
            assert!(summary.floor_stall.is_some(), "tick {n}");
            assert!(note.contains("DISPATCH CONTINUES"), "tick {n}: {note}");
            summary.floor_alerted
        })
        .collect();
    assert_eq!(alerted, [true, false]);
}

/// #10866 item 4: a tick whose release resolution fails keeps the standing
/// stall and its note, and is not a new alert.
#[test]
fn an_unresolved_tick_keeps_the_unsatisfiable_floor_alert() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Unsatisfiable);
    let (first, _) = tick(&mut state, current());
    assert!(first.floor_alerted);

    let (second, note) = tick(&mut state, unresolved());
    assert_eq!(second.floor_stall, first.floor_stall);
    assert!(second.floor_stall.is_some());
    assert!(note.contains("FLEET FLOOR UNSATISFIABLE"), "{note}");
    assert!(note.contains("DISPATCH CONTINUES"), "{note}");
    assert!(!second.floor_alerted);
}

/// #10866 item 3: a latest release whose version the floor's parser rejects
/// is "unresolved", never an unsatisfiable floor.
#[test]
fn an_unparseable_latest_version_is_unresolved_not_a_stall() {
    for floor in [Floor::Below, Floor::Unsatisfiable] {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = floored(tmp.path(), floor);
        let odd = resolved(artifact("0.19.900-rc1", Some(RUNNING), Some(SHA_A), Some(SHA_A)));
        let (summary, note) = tick(&mut state, odd);
        assert_eq!(summary.floor_stall, None, "{floor:?}");
        assert!(!summary.floor_alerted, "{floor:?}");
        assert!(state.floor.stall().is_none(), "{floor:?}");
        assert!(note.contains("is below the fleet floor"), "{floor:?}: {note}");
        assert!(note.contains("0.19.900-rc1"), "{floor:?}: {note}");
        assert!(!note.contains("FLEET FLOOR UNSATISFIABLE"), "{floor:?}: {note}");
    }
}
