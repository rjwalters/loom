//! Issue #10414: what each tick reports (`auto_update.tick`), and that the
//! loop survives a panicking tick and reports one that never returns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::auto_update::supersede::ArmedRoll;
use crate::fleet_sync::FloorWake;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::MetricName;
use crate::telemetry::TelemetryRecord;

/// A probe whose artifact resolution is scripted; `panics_left` ticks panic
/// inside `resolve_artifact` first.
struct ScriptedProbe {
    artifact: ArtifactResolution,
    fetch: RebuildOutcome,
    in_flight: usize,
    panics_left: Arc<AtomicUsize>,
    resolves: Arc<AtomicUsize>,
    /// Receives the running resolve count each time `resolve_artifact` starts.
    resolve_notify: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
}

impl ScriptedProbe {
    fn new(artifact: ArtifactResolution) -> Self {
        Self {
            artifact,
            fetch: RebuildOutcome::Success,
            in_flight: 0,
            panics_left: Arc::new(AtomicUsize::new(0)),
            resolves: Arc::new(AtomicUsize::new(0)),
            resolve_notify: None,
        }
    }
}

impl AutoUpdateProbe for ScriptedProbe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        let count = self.resolves.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(tx) = &self.resolve_notify {
            let _ = tx.send(count);
        }
        if self
            .panics_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            panic!("simulated probe panic");
        }
        self.artifact.clone()
    }
    fn fetch_artifact(&mut self, _tag: &str, _low_priority: bool) -> RebuildOutcome {
        self.fetch.clone()
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
        self.in_flight
    }
    fn rebuild(&mut self, _low_priority: bool) -> RebuildOutcome {
        RebuildOutcome::Success
    }
}

/// A trigger that accepts every drain, optionally reporting an armed roll,
/// and reports each roll it is asked for on `arms` when set.
#[derive(Default)]
struct Trigger {
    armed: Mutex<Option<ArmedRoll>>,
    arms: Option<tokio::sync::mpsc::UnboundedSender<RollTarget>>,
}

impl RollTrigger for Trigger {
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool {
        if let Some(tx) = &self.arms {
            let _ = tx.send(target.clone());
        }
        true
    }
    fn roll_in_progress(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    fn armed_roll(&self) -> Option<ArmedRoll> {
        self.armed.lock().unwrap().clone()
    }
}

fn newer(version: &str, installed: &str) -> ArtifactResolution {
    ArtifactResolution::Resolved(ArtifactInfo {
        repo: "rjwalters/loom".to_string(),
        tag: format!("v{version}"),
        version: version.to_string(),
        published_at: Some("2026-10-05T11:18:03Z".to_string()),
        asset_sha256: Some("a".repeat(64)),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        installed_version: Some(installed.to_string()),
        installed_sha256: Some("b".repeat(64)),
        on_disk_version: None,
    })
}

fn state(dir: &tempfile::TempDir) -> AutoUpdateState {
    AutoUpdateState::new_with_record_path(Some(dir.path().join("roll.json")))
}

fn tuning(interval: Duration, settle: Duration) -> TickTuning {
    TickTuning {
        interval,
        settle,
        defer_deadline: Duration::from_secs(3600),
    }
}

/// A floor feed with a fixed reading and a wake of its own, so no test
/// depends on (or disturbs) fleet-sync's process-wide floor.
fn feed(knowledge: crate::fleet_sync::FloorKnowledge) -> (FloorFeed, &'static FloorWake) {
    let wake: &'static FloorWake = Box::leak(Box::new(FloorWake::new()));
    (FloorFeed::new(move || knowledge.clone(), wake), wake)
}

fn no_store() -> FloorFeed {
    feed(crate::fleet_sync::FloorKnowledge::NoStore).0
}

const NOW: Duration = Duration::from_secs(0);

#[test]
fn an_up_to_date_host_reports_skip() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.731"));
    probe.artifact = match probe.artifact {
        ArtifactResolution::Resolved(mut info) => {
            info.installed_sha256 = info.asset_sha256.clone();
            ArtifactResolution::Resolved(info)
        }
        other => other,
    };
    let status = AutoUpdateStatus::new(true);
    let summary = run_tick(&mut state(&dir), &status, &mut probe, &Trigger::default(), NOW, NOW);
    assert_eq!(summary.decision, TickDecisionKind::Skip, "{}", summary.note);
    assert!(!summary.roll_armed);
    assert_eq!(summary.in_flight, None, "an up-to-date tick never reads in-flight");
}

#[test]
fn a_newer_release_inside_the_settle_window_reports_defer() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    let status = AutoUpdateStatus::new(true);
    let settle = Duration::from_secs(600);
    let summary = run_tick(&mut state(&dir), &status, &mut probe, &Trigger::default(), settle, NOW);
    assert_eq!(summary.decision, TickDecisionKind::Defer);
    assert!(summary.note.contains("settle"), "{}", summary.note);
    let record = tick_telemetry::record(
        &summary,
        0,
        "host-a",
        Utc::now(),
        Duration::from_millis(5),
        crate::eta::Provenance::current(),
    );
    assert_eq!(record.installed_version.as_deref(), Some("0.19.701"));
    assert_eq!(record.target_version.as_deref(), Some("0.19.731"));
    assert_eq!(record.reason, summary.note, "the defer reason is the status note");
}

#[test]
fn a_settled_newer_release_reports_fetch_and_the_armed_roll() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    probe.in_flight = 2;
    let status = AutoUpdateStatus::new(true);
    let summary = run_tick(&mut state(&dir), &status, &mut probe, &Trigger::default(), NOW, NOW);
    assert_eq!(summary.decision, TickDecisionKind::Fetch);
    assert_eq!(summary.outcome, Some("success"));
    assert!(summary.roll_armed);
    assert_eq!(summary.in_flight, Some(2));
}

#[test]
fn a_tick_with_a_roll_already_armed_reports_drain_wait_and_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    let trigger = Trigger {
        armed: Mutex::new(Some(ArmedRoll {
            target: Some(supersede::artifact_roll_target(&match newer("0.19.731", "x") {
                ArtifactResolution::Resolved(info) => info,
                ArtifactResolution::Unresolved(_) => unreachable!(),
            })),
            committed: true,
            then_exit: false,
        })),
        arms: None,
    };
    let status = AutoUpdateStatus::new(true);
    let summary = run_tick(&mut state(&dir), &status, &mut probe, &trigger, NOW, NOW);
    assert_eq!(summary.decision, TickDecisionKind::DrainWait, "{}", summary.note);
    assert!(summary.drain.armed && summary.drain.pending);
    assert_eq!(summary.drain.refusals, 0, "no deadline refusals are counted any more");
}

/// The #10414 failure mode: before the fix a panicking tick ended the loop
/// for the life of the process. Now it is a status note, a `panic` record and
/// a fault, and the state comes back for the next tick.
#[test]
fn a_panicking_tick_is_recorded_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    probe.panics_left.store(1, Ordering::SeqCst);
    let status = AutoUpdateStatus::new(true);
    let mut st = state(&dir);
    let tune = tuning(Duration::from_secs(900), NOW);
    let (summary, captured) = capture(|| {
        guarded_tick(&mut st, &status, &mut probe, &Trigger::default(), &tune, &no_store())
    });
    assert_eq!(summary.decision, TickDecisionKind::Panic);
    assert!(status.snapshot().note.unwrap().contains("tick panicked"));
    let faults: Vec<_> = captured
        .metrics
        .iter()
        .filter(|p| p.name == MetricName::DaemonTaskFaults)
        .collect();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].labels["reason"], "panic");
    let [TelemetryRecord::AutoUpdateTick(record)] = captured.records.as_slice() else {
        panic!("one auto_update.tick record, got {:?}", captured.records);
    };
    assert_eq!(record.decision, TickDecisionKind::Panic);
    assert!(record.has_provenance());

    // The next tick runs normally on the same state.
    let (next, _) = capture(|| {
        guarded_tick(&mut st, &status, &mut probe, &Trigger::default(), &tune, &no_store())
    });
    assert_eq!(next.decision, TickDecisionKind::Fetch);
}

/// Every completed tick emits exactly one record, carrying the build's
/// provenance.
#[test]
fn every_tick_emits_one_record_with_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    let status = AutoUpdateStatus::new(true);
    let tune = tuning(Duration::from_secs(900), Duration::from_secs(600));
    let mut st = state(&dir);
    let (_, captured) = capture(|| {
        guarded_tick(&mut st, &status, &mut probe, &Trigger::default(), &tune, &no_store())
    });
    let [TelemetryRecord::AutoUpdateTick(record)] = captured.records.as_slice() else {
        panic!("one auto_update.tick record");
    };
    assert_eq!(record.decision, TickDecisionKind::Defer);
    assert_eq!(record.loom, crate::eta::Provenance::current());
    assert_eq!(record.tick_id.len(), 32);
}

/// The loop keeps ticking past a panicking tick (before #10414 it returned).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loop_keeps_ticking_after_a_panicking_tick() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(ArtifactResolution::Unresolved("none".to_string()));
    let (resolve_tx, mut resolve_rx) = tokio::sync::mpsc::unbounded_channel();
    probe.resolve_notify = Some(resolve_tx);
    probe.panics_left.store(2, Ordering::SeqCst);
    let resolves = probe.resolves.clone();
    let status = Arc::new(AutoUpdateStatus::new(true));
    let tune = tuning(Duration::from_millis(20), NOW);
    let handle = tokio::spawn(run_loop(
        state(&dir),
        probe,
        Trigger::default(),
        status.clone(),
        tune,
        no_store(),
    ));
    // Ticks run serially, so the fifth resolve starting proves ticks 1-4 (two
    // panics, then two normal) each completed and published their status. The
    // timeout is only a hang guard, never the pass condition.
    let waited = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(count) = resolve_rx.recv().await {
            if count >= 5 {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(waited, Ok(true), "the loop reached a fifth tick after the two panics");
    assert!(!handle.is_finished(), "the loop is still running");
    handle.abort();
    assert!(
        resolves.load(Ordering::SeqCst) >= 5,
        "ticks after the two panics ran: {}",
        resolves.load(Ordering::SeqCst)
    );
    assert!(
        !status.snapshot().note.unwrap().contains("panicked"),
        "the latest tick was normal"
    );
}

/// A tick that runs past its bound is reported once as an overrun, and the
/// loop still collects its result when it does return.
#[test]
fn a_tick_past_its_bound_counts_one_overrun() {
    let (result, captured) = capture(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let handle = tokio::task::spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(200));
                7
            });
            wait_for_tick(handle, Duration::from_millis(20), Duration::from_millis(50)).await
        })
    });
    assert_eq!(result.unwrap(), 7);
    let overruns = captured
        .metrics
        .iter()
        .filter(|p| p.name == MetricName::DaemonTaskFaults && p.labels["reason"] == "overrun")
        .count();
    assert_eq!(overruns, 1);
}

#[test]
fn the_liveness_window_covers_one_full_tick() {
    let interval = Duration::from_secs(900);
    assert_eq!(
        liveness_window(interval),
        crate::task_liveness::default_stale_after(interval) + TICK_OVERRUN
    );
    assert!(TICK_OVERRUN > DEFAULT_REBUILD_TIMEOUT);
}

/// The loom-worker-1 shape on 2026-10-05: `settleSecs = 14400` with releases
/// landing every 10–60 min. The quiet period never elapses, so the note must
/// name the ceiling: that is when the host will actually roll.
#[test]
fn a_settle_defer_names_the_quiet_period_and_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(newer("0.19.731", "0.19.701"));
    let status = AutoUpdateStatus::new(true);
    let settle = Duration::from_secs(14_400);
    let summary = run_tick(&mut state(&dir), &status, &mut probe, &Trigger::default(), settle, NOW);
    assert_eq!(summary.decision, TickDecisionKind::Defer);
    assert!(summary.note.starts_with("within settle window"), "{}", summary.note);
    assert!(summary.note.contains("quiet period 14400s"), "{}", summary.note);
    assert!(
        summary
            .note
            .contains("settle ceiling forces the roll in ~86400s")
            || summary
                .note
                .contains("settle ceiling forces the roll in ~86399s"),
        "6 x 14400 s from the first observation: {}",
        summary.note
    );
}

/// #10885: the loop's first tick runs at spawn, not one interval later, so a
/// host that starts below its floor arms its pause-and-roll on that tick. The
/// interval here is an hour; the timeout is only a hang guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_starts_below_its_floor_arms_on_the_startup_tick() {
    let dir = tempfile::tempdir().unwrap();
    let probe = ScriptedProbe::new(newer("999.0.1", env!("CARGO_PKG_VERSION")));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let trigger = Trigger {
        arms: Some(tx),
        ..Trigger::default()
    };
    let (floor, _) = feed(crate::fleet_sync::FloorKnowledge::Set("999.0.0".to_string()));
    let status = Arc::new(AutoUpdateStatus::new(true));
    // A week of settle: only a floor roll ignores it.
    let tune = tuning(Duration::from_secs(3600), Duration::from_secs(7 * 86_400));
    let handle = tokio::spawn(run_loop(state(&dir), probe, trigger, status, tune, floor));
    let armed = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await;
    handle.abort();
    let target = armed
        .expect("armed on the startup tick")
        .expect("a roll target");
    assert_eq!(target.source, pause_manifest::TargetSource::Floor);
    assert_eq!(target.to_version.as_deref(), Some("999.0.1"));
}

/// #10885: a floor change wakes the loop at once instead of at the next
/// interval (an hour here), and a wake with no change behind it does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_floor_change_ticks_now_and_no_change_waits_for_the_interval() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = ScriptedProbe::new(ArtifactResolution::Unresolved("none".to_string()));
    let (resolve_tx, mut resolves) = tokio::sync::mpsc::unbounded_channel();
    probe.resolve_notify = Some(resolve_tx);
    let (floor, wake) = feed(crate::fleet_sync::FloorKnowledge::NoStore);
    let status = Arc::new(AutoUpdateStatus::new(true));
    let tune = tuning(Duration::from_secs(3600), NOW);
    let handle =
        tokio::spawn(run_loop(state(&dir), probe, Trigger::default(), status.clone(), tune, floor));
    let guard = Duration::from_secs(60);
    let first = tokio::time::timeout(guard, resolves.recv()).await;
    assert_eq!(first, Ok(Some(1)), "the startup tick");
    // Let the startup tick finish, then check nothing else is coming.
    tokio::time::timeout(guard, async {
        while status.snapshot().note.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the startup tick published");
    let idle = tokio::time::timeout(Duration::from_millis(300), resolves.recv()).await;
    assert!(idle.is_err(), "no tick without a floor change before the interval");

    wake.bump();
    let woken = tokio::time::timeout(guard, resolves.recv()).await;
    handle.abort();
    assert_eq!(woken, Ok(Some(2)), "the floor change ticked the loop at once");
}
