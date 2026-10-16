//! Issue #12425: restart-required config arms the one pause-and-roll, onto the
//! binary already running.
//!
//! Driven through `guarded_tick`, so the feed, the loop-mode gate and the
//! `auto_update.tick` record are all in the path. A sibling module rather than
//! more lines in `tests.rs`, per `.loom/docs/file-size-policy.md`.

use super::*;
use crate::auto_update::runner::{guarded_tick, FloorFeed};
use crate::auto_update::supersede::ArmedRoll;
use crate::auto_update::tick_telemetry::TickSummary;
use crate::fleet_sync::config_restart::ConfigRestartDemand;
use crate::fleet_sync::{FloorKnowledge, FloorWake};
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;
use std::sync::Mutex;

const RUNNING: &str = env!("CARGO_PKG_VERSION");

fn demand(paths: &[&str]) -> ConfigRestartDemand {
    ConfigRestartDemand {
        paths: paths.iter().map(ToString::to_string).collect(),
        fingerprint: "0".repeat(64),
        first_seen: Utc::now(),
    }
}

fn feed(knowledge: FloorKnowledge, demand: Option<ConfigRestartDemand>) -> FloorFeed {
    let wake: &'static FloorWake = Box::leak(Box::new(FloorWake::new()));
    let feed = FloorFeed::new(move || knowledge.clone(), wake);
    match demand {
        Some(demand) => feed.with_config_restart(demand),
        None => feed,
    }
}

/// `chase_enabled = false` is the issue's "even with autoUpdate off".
fn tuning() -> TickTuning {
    TickTuning {
        interval: Duration::from_secs(900),
        settle: Duration::from_secs(600),
        defer_deadline: DEFER,
        chase_enabled: false,
    }
}

/// Records every `trigger_pause_roll` target, and whether the persisted state
/// file already existed when it was called.
struct Trigger {
    armed: Mutex<Option<ArmedRoll>>,
    targets: Mutex<Vec<RollTarget>>,
    state_file: Option<PathBuf>,
    state_written_first: Mutex<Vec<bool>>,
    accept: bool,
}

impl Trigger {
    fn new() -> Self {
        Self {
            armed: Mutex::new(None),
            targets: Mutex::new(Vec::new()),
            state_file: None,
            state_written_first: Mutex::new(Vec::new()),
            accept: true,
        }
    }
}

impl RollTrigger for Trigger {
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool {
        self.targets.lock().unwrap().push(target.clone());
        if let Some(file) = &self.state_file {
            self.state_written_first.lock().unwrap().push(file.exists());
        }
        self.accept
    }
    fn roll_in_progress(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
}

fn probe(installed: &str, fetch_calls: &Arc<AtomicUsize>) -> ArtifactFakeProbe {
    ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.999999", Some(installed), Some(SHA_B), Some(SHA_A))),
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
        rebuild_calls: Arc::new(AtomicUsize::new(0)),
    }
}

struct Run {
    summary: TickSummary,
    trigger: Trigger,
    fetches: usize,
    state: AutoUpdateState,
}

fn tick_with(
    floor: FloorKnowledge,
    config: Option<ConfigRestartDemand>,
    trigger: Trigger,
    state_dir: Option<&Path>,
) -> Run {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());
    if let Some(dir) = state_dir {
        state.attach_persistence(Some(dir.join("auto_update_state.json")));
    }
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    // A newer release exists, but a fleet host rolls only for its floor.
    let mut probe = probe(RUNNING, &fetch_calls);
    let status = AutoUpdateStatus::new(true);
    let summary =
        guarded_tick(&mut state, &status, &mut probe, &trigger, &tuning(), &feed(floor, config));
    Run {
        summary,
        trigger,
        fetches: fetch_calls.load(Ordering::SeqCst),
        state,
    }
}

fn tick(floor: FloorKnowledge, config: Option<ConfigRestartDemand>) -> Run {
    tick_with(floor, config, Trigger::new(), None)
}

fn met() -> FloorKnowledge {
    FloorKnowledge::Set("0.19.1".to_string())
}

#[test]
fn a_standing_demand_arms_one_same_binary_restart_with_autoupdate_off() {
    let run = tick(met(), Some(demand(&["a.restart", "b.restart"])));
    let targets = run.trigger.targets.lock().unwrap().clone();
    assert_eq!(targets.len(), 1, "{}", run.summary.note);
    let target = &targets[0];
    assert_eq!(target.source, pause_manifest::TargetSource::ConfigRestart);
    assert_eq!(target.to_version.as_deref(), Some(RUNNING));
    assert_eq!((target.to_artifact_sha256.as_deref(), target.label.as_deref()), (None, None));
    assert_eq!(run.fetches, 0, "no fetch");
    assert_eq!(run.summary.decision, TickDecisionKind::ConfigRestart);
    assert!(run.summary.roll_armed);
    assert!(run.summary.note.contains("a.restart, b.restart"), "{}", run.summary.note);
    // Several keys, one restart, one count.
    let facts = run.summary.config_restart.unwrap();
    assert_eq!((facts.source, facts.paths), ("config_restart", 2));
}

#[test]
fn no_demand_arms_nothing() {
    let run = tick(met(), None);
    assert!(run.trigger.targets.lock().unwrap().is_empty());
    assert_eq!(run.summary.decision, TickDecisionKind::Skip);
    assert_eq!(run.summary.config_restart, None);
}

#[test]
fn a_host_with_no_fleet_store_arms_nothing() {
    let run = tick(FloorKnowledge::NoStore, Some(demand(&["a.restart"])));
    assert!(run.trigger.targets.lock().unwrap().is_empty());
    assert_ne!(run.summary.decision, TickDecisionKind::ConfigRestart);
}

#[test]
fn a_version_roll_arming_keeps_the_tick() {
    // The floor is above the running version and a release meets it.
    let run = tick(FloorKnowledge::Set("0.19.999999".to_string()), Some(demand(&["a.restart"])));
    assert_ne!(run.summary.decision, TickDecisionKind::ConfigRestart);
    assert!(
        run.trigger
            .targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.source != pause_manifest::TargetSource::ConfigRestart),
        "{}",
        run.summary.note
    );
}

#[test]
fn an_armed_roll_is_left_to_finish() {
    let trigger = Trigger::new();
    *trigger.armed.lock().unwrap() = Some(ArmedRoll {
        target: Some("v0.19.850@cccc".to_string()),
        committed: true,
        then_exit: false,
    });
    let run = tick_with(met(), Some(demand(&["a.restart"])), trigger, None);
    assert_eq!(run.summary.decision, TickDecisionKind::DrainWait);
    assert!(run.trigger.targets.lock().unwrap().is_empty());
}

/// The same-binary restart changes nothing a version roll records, and writes
/// the state it restores (#10877) before the roll may restart at once.
#[test]
fn it_records_no_version_roll_and_persists_state_before_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut trigger = Trigger::new();
    trigger.state_file = Some(dir.path().join("auto_update_state.json"));
    let run = tick_with(met(), Some(demand(&["a.restart"])), trigger, Some(dir.path()));
    assert_eq!(*run.trigger.state_written_first.lock().unwrap(), vec![true]);
    assert_eq!(run.state.last_roll, None);
    assert!(run.state.last_artifact_roll.is_none());
    assert_eq!(run.state.consecutive_failures, 0);
    assert!(run.state.backoff_until.is_none());
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("auto_update_state.json")).unwrap(),
    )
    .unwrap();
    assert!(saved["roll_attempt"].is_null(), "no roll attempt is armed: {saved}");
}

#[test]
fn a_refused_restart_is_retried_by_the_next_tick() {
    let mut trigger = Trigger::new();
    trigger.accept = false;
    let run = tick_with(met(), Some(demand(&["a.restart"])), trigger, None);
    assert_eq!(run.summary.decision, TickDecisionKind::ConfigRestart);
    assert!(!run.summary.roll_armed);
    assert!(run.summary.note.contains("refused"), "{}", run.summary.note);
}

#[test]
fn the_record_carries_the_source_and_a_path_count_never_the_paths() {
    let run = tick(met(), Some(demand(&["secret.looking.path", "b.restart"])));
    let record = crate::auto_update::tick_telemetry::record(
        &run.summary,
        0,
        "host",
        Utc::now(),
        Duration::from_millis(1),
        crate::telemetry::provenance::Provenance::current(),
    );
    assert_eq!(record.decision, TickDecisionKind::ConfigRestart);
    assert_eq!(record.target_source.as_deref(), Some("config_restart"));
    assert_eq!(record.config_restart_paths, Some(2));
}
