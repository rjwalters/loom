//! Issue #10720: the self-update tick's `config_restart` trigger, against a
//! fake trigger, and the restart it causes against persisted state.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};

use super::super::super::*;
use super::super::{guarded_tick, FloorFeed};
use super::*;
use crate::auto_update::floor_roll::alert::FloorStallState;
use crate::auto_update::pause_manifest::TargetSource;
use crate::auto_update::supersede::ArmedRoll;
use crate::fleet_sync::{ConfigRestartDemand, FloorKnowledge, FloorWake};
use crate::observability::ops::capture::capture;
use crate::telemetry::TelemetryRecord;

/// A probe whose artifact is fixed.
struct Probe(ArtifactResolution);

impl AutoUpdateProbe for Probe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        self.0.clone()
    }
    fn fetch_artifact(&mut self, _tag: &str, _low_priority: bool) -> RebuildOutcome {
        RebuildOutcome::Success
    }
    fn check(&self) -> UpdateCheck {
        UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        }
    }
    fn is_tree_clean(&self) -> Option<bool> {
        Some(true)
    }
    fn in_flight_sweeps(&self) -> usize {
        3
    }
    fn rebuild(&mut self, _low_priority: bool) -> RebuildOutcome {
        RebuildOutcome::Success
    }
}

/// A trigger that records every roll it is asked for. Once one is armed it
/// reports a roll in progress, as the production trigger does.
#[derive(Default)]
struct Trigger {
    arms: Mutex<Vec<RollTarget>>,
    armed: Mutex<Option<ArmedRoll>>,
    refuse: bool,
}

impl RollTrigger for Trigger {
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool {
        self.arms.lock().unwrap().push(target.clone());
        if self.refuse {
            return false;
        }
        *self.armed.lock().unwrap() = Some(ArmedRoll {
            target: target.label.clone(),
            committed: false,
            then_exit: false,
        });
        true
    }
    fn roll_in_progress(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
}

impl Trigger {
    fn arms(&self) -> Vec<RollTarget> {
        self.arms.lock().unwrap().clone()
    }
}

const RUNNING: &str = env!("CARGO_PKG_VERSION");

fn release(version: &str) -> ArtifactResolution {
    ArtifactResolution::Resolved(ArtifactInfo {
        repo: "rjwalters/loom".to_string(),
        tag: format!("v{version}"),
        version: version.to_string(),
        published_at: Some("2026-10-11T00:00:00Z".to_string()),
        asset_sha256: Some("a".repeat(64)),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        installed_version: Some(RUNNING.to_string()),
        installed_sha256: Some("a".repeat(64)),
        on_disk_version: None,
    })
}

fn running_plus(by: u64) -> String {
    let (head, patch) = RUNNING.rsplit_once('.').unwrap();
    format!("{head}.{}", patch.parse::<u64>().unwrap() + by)
}

fn demand(armable: bool) -> ConfigRestartDemand {
    ConfigRestartDemand {
        paths: vec![
            "autonomous.hostBreaker.enabled".to_string(),
            "forge.githubApp.keyPath".to_string(),
        ],
        fingerprint: "f".repeat(64),
        first_seen: Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap(),
        armable,
    }
}

/// A fleet host's feed: the floor is `floor`, and `demand` stands.
fn feed(floor: &str, demand: ConfigRestartDemand) -> FloorFeed {
    let wake: &'static FloorWake = Box::leak(Box::new(FloorWake::new()));
    let knowledge = FloorKnowledge::Set(floor.to_string());
    FloorFeed::new(move || knowledge.clone(), wake).with_config_demand(demand)
}

/// A loop spawned on a fleet host with `autonomous.autoUpdate.enabled` off.
fn fleet_tuning() -> TickTuning {
    TickTuning {
        interval: Duration::from_secs(900),
        settle: Duration::from_secs(600),
        defer_deadline: Duration::from_secs(3600),
        chase_enabled: false,
    }
}

struct Run {
    _dir: tempfile::TempDir,
    state: AutoUpdateState,
    status: AutoUpdateStatus,
}

impl Run {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = AutoUpdateState::new_with_record_path(Some(dir.path().join("roll.json")));
        Self {
            _dir: dir,
            state,
            status: AutoUpdateStatus::new(false),
        }
    }

    fn tick(
        &mut self,
        trigger: &Trigger,
        artifact: ArtifactResolution,
        feed: &FloorFeed,
    ) -> (TickSummary, Vec<TelemetryRecord>) {
        let mut probe = Probe(artifact);
        let (summary, captured) = capture(|| {
            guarded_tick(&mut self.state, &self.status, &mut probe, trigger, &fleet_tuning(), feed)
        });
        (summary, captured.records)
    }
}

#[test]
fn decide_reports_folds_or_arms() {
    let idle = Trigger::default();
    assert_eq!(decide(&demand(false), false, &idle), Action::Report);
    assert!(matches!(decide(&demand(true), true, &idle), Action::Fold(_)));
    assert_eq!(decide(&demand(true), false, &idle), Action::Arm);
    let busy = Trigger::default();
    *busy.armed.lock().unwrap() = Some(ArmedRoll {
        target: None,
        committed: true,
        then_exit: true,
    });
    let Action::Fold(why) = decide(&demand(true), false, &busy) else {
        panic!("an operator stop in progress folds the demand");
    };
    assert!(why.contains("operator stop"), "{why}");
}

/// A standing demand on a fleet host with autoUpdate off arms exactly one
/// pause-and-roll, onto the running version, with source `config_restart`;
/// the next tick folds into it instead of arming a second.
#[test]
fn a_standing_demand_arms_exactly_one_pause_roll_on_the_next_tick() {
    let mut run = Run::new();
    let trigger = Trigger::default();
    let feed = feed(RUNNING, demand(true));
    let (summary, records) = run.tick(&trigger, release(RUNNING), &feed);
    assert_eq!(summary.decision, TickDecisionKind::ConfigRestart, "{}", summary.note);
    assert!(summary.roll_armed);
    assert_eq!(summary.config_restart_paths, Some(2));
    let [target] = trigger.arms().try_into().unwrap();
    assert_eq!(target.source, TargetSource::ConfigRestart);
    assert_eq!(target.to_version.as_deref(), Some(RUNNING));
    assert_eq!(target.label, None, "never a supersede candidate");
    assert!(run
        .status
        .snapshot()
        .note
        .unwrap()
        .contains("config restart armed"));
    // The record: decision, source and count, never a path or value.
    let tick = records
        .iter()
        .find_map(|r| match r {
            TelemetryRecord::AutoUpdateTick(t) => Some(t.clone()),
            _ => None,
        })
        .expect("one auto_update.tick record");
    assert_eq!(tick.decision, TickDecisionKind::ConfigRestart);
    assert_eq!(tick.target_source.as_deref(), Some("config_restart"));
    assert_eq!(tick.config_restart_paths, Some(2));
    let body = serde_json::to_string(&tick).unwrap();
    assert!(!body.contains("keyPath") && !body.contains("hostBreaker"), "{body}");

    let (again, _) = run.tick(&trigger, release(RUNNING), &feed);
    assert_eq!(trigger.arms().len(), 1, "no second arm while the first is in progress");
    assert_ne!(again.decision, TickDecisionKind::ConfigRestart);
    assert!(again.note.contains("folded into"), "{}", again.note);
}

/// A version roll armed on the same tick absorbs the demand: one arm, and it
/// is the version roll's.
#[test]
fn a_version_roll_this_tick_absorbs_the_demand() {
    let mut run = Run::new();
    let trigger = Trigger::default();
    let (summary, _) =
        run.tick(&trigger, release(&running_plus(2)), &feed(&running_plus(1), demand(true)));
    assert_eq!(summary.decision, TickDecisionKind::Fetch, "{}", summary.note);
    let [target] = trigger.arms().try_into().unwrap();
    assert_eq!(target.source, TargetSource::Floor);
    assert!(summary.note.contains("folded into the version roll"), "{}", summary.note);
    assert_eq!(summary.config_restart_paths, Some(2));
}

/// A version roll already in progress absorbs it too.
#[test]
fn a_roll_already_in_progress_absorbs_the_demand() {
    let mut run = Run::new();
    let trigger = Trigger::default();
    *trigger.armed.lock().unwrap() = Some(ArmedRoll {
        target: Some("artifact:9.9.9:x".to_string()),
        committed: false,
        then_exit: false,
    });
    let (summary, _) = run.tick(&trigger, release(RUNNING), &feed(RUNNING, demand(true)));
    assert!(trigger.arms().is_empty());
    assert_eq!(summary.decision, TickDecisionKind::DrainWait, "{}", summary.note);
    assert!(summary.note.contains("folded into"), "{}", summary.note);
}

/// A `Mode::Check` host reports and arms nothing.
#[test]
fn a_check_mode_demand_is_reported_and_never_armed() {
    let mut run = Run::new();
    let trigger = Trigger::default();
    let (summary, _) = run.tick(&trigger, release(RUNNING), &feed(RUNNING, demand(false)));
    assert!(trigger.arms().is_empty());
    assert!(!summary.roll_armed);
    assert!(summary.note.contains("nothing is armed"), "{}", summary.note);
    assert_eq!(summary.config_restart_paths, Some(2));
}

/// A trigger that cannot start (no supervisor) leaves the demand for the next
/// tick, which tries again.
#[test]
fn a_refused_arm_is_retried_on_the_next_tick() {
    let mut run = Run::new();
    let trigger = Trigger {
        refuse: true,
        ..Trigger::default()
    };
    let feed = feed(RUNNING, demand(true));
    let (summary, _) = run.tick(&trigger, release(RUNNING), &feed);
    assert!(!summary.roll_armed);
    assert!(summary.note.contains("could not start"), "{}", summary.note);
    let _ = run.tick(&trigger, release(RUNNING), &feed);
    assert_eq!(trigger.arms().len(), 2);
}

/// No demand: nothing changes on the tick.
#[test]
fn no_demand_leaves_the_tick_alone() {
    let mut run = Run::new();
    let trigger = Trigger::default();
    let wake: &'static FloorWake = Box::leak(Box::new(FloorWake::new()));
    let feed = FloorFeed::new(|| FloorKnowledge::Set(RUNNING.to_string()), wake);
    let (summary, _) = run.tick(&trigger, release(RUNNING), &feed);
    assert!(trigger.arms().is_empty());
    assert_eq!(summary.config_restart_paths, None);
    assert!(!summary.note.contains("config restart"), "{}", summary.note);
}

/// The restart: state persisted before the arm and read back by a fresh state
/// with the same binary keeps its settle clocks and stall episode; the config
/// restart stamps no `last_roll` and writes no artifact-roll record.
#[test]
fn a_config_restart_keeps_the_same_binary_state_and_is_not_a_completed_roll() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join(persisted_state::STATE_FILE);
    let record_path = dir.path().join("roll.json");
    let mut before = AutoUpdateState::new_with_record_path(Some(record_path.clone()));
    before.attach_persistence(Some(state_path.clone()));
    let now = Instant::now();
    before.tracked_target = Some("artifact:0.0.1:abc".to_string());
    before.stale_since = Some(now);
    before.first_stale_since = Some(now);
    let stall = FloorStallState {
        floor: "9.9.9".to_string(),
        running: RUNNING.to_string(),
        newest: RUNNING.to_string(),
        declared_at: Utc.with_ymd_and_hms(2026, 10, 11, 1, 0, 0).unwrap(),
        last_alerted_at: Utc.with_ymd_and_hms(2026, 10, 11, 2, 0, 0).unwrap(),
    };
    before.floor.restore_stall_state(Some(stall.clone()));
    let trigger = Trigger::default();
    let status = AutoUpdateStatus::new(false);
    let summary = TickSummary::new(None);
    let summary = after_tick(&before, &status, &trigger, Some(&demand(true)), summary);
    assert!(summary.roll_armed);
    assert!(state_path.exists(), "persisted before the arm");
    assert_eq!(before.last_roll, None, "not a completed roll");
    assert!(!record_path.exists(), "no artifact-roll record");
    assert_eq!(before.consecutive_failures, 0);

    // The restarted process: same binary, state read back from disk.
    let mut after = AutoUpdateState::new_with_record_path(Some(record_path.clone()));
    after.attach_persistence(Some(state_path));
    assert_eq!(after.tracked_target.as_deref(), Some("artifact:0.0.1:abc"));
    assert!(after.first_stale_since.is_some(), "the settle ceiling is carried");
    assert!(after.stale_since.is_some());
    assert_eq!(after.floor.stall_state(), Some(stall), "the stall episode is carried");
    assert_eq!(after.last_roll, None);
    assert!(!record_path.exists());
}
