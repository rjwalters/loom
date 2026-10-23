//! Host maintenance (#11812): `restart --pause --then-exit` takes over
//! whatever holds dispatch without reopening it, runs the roll's own H4, and
//! ends in stay-down. Scripted host, as in `tests.rs`.

use super::super::tests::{cand, load, tuning, FakeHost};
use super::super::*;
use crate::auto_update::supersede::{self, ArmedRollAction};
use crate::auto_update::{ArtifactInfo, ArtifactResolution};
use crate::event_bus::EventBus;
use crate::ipc::{DrainBegin, DrainOrigin, MaintenanceBegin, PauseRollStatus, TookOver};
use crate::workspace_pool::WorkspacePool;
use std::sync::atomic::{AtomicBool, Ordering};

fn progress() -> PauseRollStatus {
    PauseRollStatus {
        budget_secs: 1,
        target_source: Some("host-maintenance".to_string()),
        ..PauseRollStatus::default()
    }
}

/// Begin host maintenance on `drain`, asserting it started, and the plan.
fn begin(drain: &DrainState, dir: &Path, t: PauseRollTuning) -> (PausePlan, Option<TookOver>) {
    let MaintenanceBegin::Started {
        generation,
        took_over,
    } = drain.begin_host_maintenance(t.pause_budget, progress())
    else {
        panic!("host maintenance must start");
    };
    let plan = PausePlan {
        target: RollTarget::host_maintenance(),
        tuning: t,
        manifest_path: dir.join("state").join(pause_manifest::MANIFEST_FILE),
        from_version: "0.19.1155".to_string(),
        generation,
        staged_at: Utc::now(),
        supervisor: Some("systemd".to_string()),
    };
    (plan, took_over)
}

/// Sample the dispatch-pause flag on another thread until told to stop;
/// returns whether it was ever seen clear.
fn watch_flag(drain: &Arc<DrainState>) -> (Arc<AtomicBool>, std::thread::JoinHandle<bool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (d, s) = (drain.clone(), stop.clone());
    let handle = std::thread::spawn(move || {
        let mut opened = false;
        while !s.load(Ordering::SeqCst) {
            opened |= !d.is_draining();
        }
        opened
    });
    (stop, handle)
}

fn assert_pauses_and_stays_down(drain: &Arc<DrainState>, dir: &Path, plan: &PausePlan) {
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir, "old", 1, 900), cand(dir, "young", 2, 5)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();
    let parker = super::super::tests::spawn_parking_agent(pause_dir);
    let outcome = run_h4(drain, host.clone(), plan);
    parker.join().unwrap();
    let H4Outcome::Paused { then_exit, .. } = outcome else {
        panic!("expected Paused, got {outcome:?}");
    };
    assert!(then_exit, "host maintenance stays down");
    assert_eq!(crate::ipc::drain_exit_code(then_exit), crate::ipc::EXIT_SHUTDOWN);
    let m = load(plan);
    assert_eq!(m.phase, Phase::Paused);
    assert_eq!(m.roll.target_source, Some(TargetSource::HostMaintenance));
    assert_eq!(m.roll.max_age_secs, 900, "the manifest max age is the lease TTL");
    let old = m.items.iter().find(|i| i.id == "old").unwrap();
    assert_eq!((&old.disposition, &old.status), (&Disposition::Resume, &ItemStatus::Paused));
    assert_eq!(host.leases.lock().unwrap().as_slice(), ["old".to_string()], "lease refreshed");
    let events = host.events.lock().unwrap();
    let paused = events
        .iter()
        .find(|(t, _)| t == "daemon.roll.paused")
        .unwrap();
    assert_eq!(paused.1["trigger"], "host-maintenance");
    assert_eq!(paused.1["then_exit"], true);
    assert!(events
        .iter()
        .filter(|(t, _)| t == "daemon.roll.item")
        .all(|(_, p)| p["trigger"] == "host-maintenance"));
    assert!(drain.is_draining(), "dispatch is still closed as the daemon exits");
}

/// (a) The resize case: an operator `--drain --then-exit` still waiting for
/// stragglers is converted to H4 in place. Dispatch is never open, the drain
/// supervisor stands down without exiting, and H4 ends in stay-down.
#[tokio::test(flavor = "multi_thread")]
async fn an_in_progress_operator_then_exit_drain_is_taken_over_without_opening_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, "started_at=2026-10-13T00:00:00Z\n").unwrap();
    let drain = Arc::new(DrainState::new().with_stop_marker(marker.clone()));
    let DrainBegin::Started {
        generation: drain_gen,
        ..
    } = drain.begin(Duration::from_secs(1800), false, true)
    else {
        panic!("the operator drain starts");
    };
    let (stop, watcher) = watch_flag(&drain);

    let (plan, took_over) = begin(&drain, dir.path(), tuning(3000));

    assert_eq!(took_over, Some(TookOver::Drain { then_exit: true }));
    assert_ne!(plan.generation, drain_gen, "the drain supervisor's generation is retired");
    let snap = drain.snapshot();
    assert!(
        snap.active && snap.then_exit && snap.origin == DrainOrigin::PauseRoll,
        "{snap:?}"
    );
    assert!(!snap.timed_out && !snap.force_after_timeout);
    assert!(crate::operator_stop::is_recorded(&marker), "the operator stop stays on record");

    // The old wait-for-zero supervisor returns at once, without exiting.
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    crate::ipc::drain_supervisor::run_drain_supervisor(
        drain.clone(),
        pool,
        dir.path().join("none"),
        bus,
        drain_gen,
        Duration::from_millis(10),
    )
    .await;

    let (d, p, root) = (drain.clone(), plan.clone(), dir.path().to_path_buf());
    tokio::task::spawn_blocking(move || assert_pauses_and_stays_down(&d, &root, &p))
        .await
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    assert!(!watcher.join().unwrap(), "dispatch was never open, not for a moment");
}

/// (a) The same for a drain that timed out and holds dispatch paused (#9588).
#[test]
fn a_timed_out_held_drain_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let _ = drain.begin(Duration::from_secs(1), false, true);
    drain.hold_after_timeout("TIMED OUT, dispatch held PAUSED".to_string());
    let (stop, watcher) = watch_flag(&drain);

    let (plan, took_over) = begin(&drain, dir.path(), tuning(3000));

    assert_eq!(took_over, Some(TookOver::TimedOutDrain { then_exit: true }));
    assert!(!drain.snapshot().timed_out);
    assert_pauses_and_stays_down(&drain, dir.path(), &plan);
    stop.store(true, Ordering::SeqCst);
    assert!(!watcher.join().unwrap(), "dispatch was never open");
}

/// A relaunch drain, a startup hold and a fleet hold are taken over too; with
/// nothing active a fresh pause starts. Every one ends then-exit.
#[test]
fn holds_and_relaunch_drains_are_taken_over_and_an_idle_host_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let relaunch = DrainState::new();
    let _ = relaunch.begin(Duration::from_secs(60), false, false);
    assert_eq!(
        begin(&relaunch, dir.path(), tuning(100)).1,
        Some(TookOver::Drain { then_exit: false })
    );
    assert!(relaunch.then_exit(), "then-exit is one-way");

    let fleet = DrainState::new();
    assert!(fleet.hold_for_fleet_state("fleet state: paused".to_string()));
    assert_eq!(begin(&fleet, dir.path(), tuning(100)).1, Some(TookOver::FleetHold));
    assert!(!fleet.is_fleet_held(), "the store flipping to running cannot release it");
    assert!(!fleet.release_fleet_hold() && fleet.is_draining());

    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, "x\n").unwrap();
    crate::operator_stop::record(&marker, "restart --drain --then-exit").unwrap();
    let startup = DrainState::new().with_stop_marker(marker);
    assert_eq!(begin(&startup, dir.path(), tuning(100)).1, Some(TookOver::StartupHold));

    let idle = DrainState::new();
    assert_eq!(begin(&idle, dir.path(), tuning(100)).1, None);
    let snap = idle.snapshot();
    assert!(idle.is_draining() && snap.then_exit && snap.origin == DrainOrigin::PauseRoll);
}

/// H5 still resuming a previous pause: refused, nothing changed.
#[test]
fn it_is_refused_while_h5_is_resuming() {
    let drain = DrainState::new();
    assert!(drain.hold_for_roll_resume("resuming".to_string()));
    let MaintenanceBegin::Refused(why) =
        drain.begin_host_maintenance(Duration::from_secs(1), progress())
    else {
        panic!("must be refused during H5");
    };
    assert!(why.contains("H5"), "{why}");
    assert!(drain.is_roll_resume_held() && !drain.then_exit());
}

/// A version roll already in H4 is escalated, not doubled: it keeps its
/// generation and stays down at the end, and the auto-updater can no longer
/// supersede it (the supersede rules treat a then-exit as never-supersede).
#[test]
fn a_pause_roll_in_h4_is_escalated_and_can_no_longer_be_superseded() {
    let drain = DrainState::new();
    let DrainBegin::Started { generation, .. } =
        drain.begin_pause_roll(Duration::from_secs(120), PauseRollStatus::default())
    else {
        panic!("the roll starts");
    };
    drain.set_roll_target(Some("v0.19.900@aaaa".to_string()));
    let MaintenanceBegin::Escalated {
        generation: escalated,
        ..
    } = drain.begin_host_maintenance(Duration::from_secs(120), progress())
    else {
        panic!("expected Escalated");
    };
    assert_eq!(escalated, generation, "the same H4 carries on");
    assert!(drain.then_exit());
    assert!(!drain.abort_pause_roll(), "the auto-updater cannot end it");
    let snap = drain.snapshot();
    let armed = supersede::ArmedRoll {
        target: snap.roll_target,
        committed: false,
        then_exit: snap.then_exit,
    };
    let newer = ArtifactResolution::Resolved(ArtifactInfo {
        tag: "v0.19.901".to_string(),
        version: "0.19.901".to_string(),
        ..ArtifactInfo::default()
    });
    assert!(matches!(
        supersede::decide_armed_roll(Some(&armed), &newer),
        ArmedRollAction::Skip(_)
    ));
}

/// A host-maintenance pause that cannot write its manifest falls back to a
/// held then-exit drain: dispatch stays paused, the daemon stops once nothing
/// is in flight. It never resumes dispatch on its own.
#[test]
fn a_failed_host_maintenance_pause_falls_back_to_a_held_then_exit_drain() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let _ = drain.begin(Duration::from_secs(1800), false, true);
    let (mut plan, _) = begin(&drain, dir.path(), tuning(500));
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    plan.manifest_path = blocker.join(pause_manifest::MANIFEST_FILE);
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });

    let outcome = run_h4(&drain, host.clone(), &plan);

    assert_eq!(outcome, H4Outcome::StoodDown { promoted: true });
    assert!(host.torn.lock().unwrap().is_empty(), "nothing was stopped");
    let snap = drain.snapshot();
    assert!(drain.is_draining(), "dispatch stays paused");
    assert!(snap.active && snap.then_exit && snap.timed_out);
    assert_eq!(snap.origin, DrainOrigin::Operator, "a drain the caller supervises");
    assert_eq!(drain.generation(), plan.generation, "for this generation");
    assert!(snap
        .note
        .unwrap()
        .contains("Fell back to a then-exit drain"));
}
