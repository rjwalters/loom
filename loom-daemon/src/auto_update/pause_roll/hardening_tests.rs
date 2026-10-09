//! Tests for the Judge's findings on #10974: a stale safe point, dispatches
//! that are mid-spawn, a pause that cannot finish, and a superseded run that
//! stands down late. They share `tests.rs`'s fixtures. Every process these
//! tests signal is one they spawned.

use super::host::{self, Candidate, PauseHost};
use super::ledger::RunLedger;
use super::supervise::end_unfinished;
use super::tests::{
    begin, cand, item, load, spawn_parking_agent, tuning, FakeAgent, FakeHost, RegistryHost,
};
use super::*;
use crate::auto_update::pause_manifest::LoadOutcome;
use crate::ipc::AbortOutcome;
use crate::sweep_registry::test_support;
use crate::sweep_registry::{poll_and_classify_spawned_child, BeginIssueDispatch};
use crate::types::SweepKind;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

fn ledger(id: &str, plan: &PausePlan) -> Arc<RunLedger> {
    Arc::new(RunLedger::new(id.to_string(), plan.manifest_path.clone()))
}

/// Run H4 on its own thread, as the supervisor does.
fn spawn_h4(
    drain: &Arc<DrainState>,
    host: Arc<dyn PauseHost>,
    plan: &PausePlan,
    ledger: &Arc<RunLedger>,
) -> std::thread::JoinHandle<H4Outcome> {
    let (d, p, l) = (drain.clone(), plan.clone(), ledger.clone());
    std::thread::spawn(move || run_h4_with(&d, host, &p, &l))
}

fn raw_manifest(plan: &PausePlan) -> PauseManifest {
    serde_json::from_str(&std::fs::read_to_string(&plan.manifest_path).unwrap()).unwrap()
}

/// What a hook parked for `request_id` leaves behind when nothing cleans up.
fn plant_safe_point(pause_dir: &Path, request_id: Option<&str>, summary: &str) {
    let record = serde_json::json!({
        "reached_at": rfc3339(Utc::now()),
        "parked_tool": "Bash",
        "parked_summary": summary,
        "parked_tool_use_id": "toolu_old",
        "runtime": "claude",
        "request_id": request_id,
    });
    std::fs::create_dir_all(pause_dir).unwrap();
    std::fs::write(pause_dir.join(roll_pause::SAFE_POINT_FILE), record.to_string()).unwrap();
}

// ============================================================================
// Finding 2: a stale safe-point record
// ============================================================================

/// A pause stands down after its agent parked; the NEXT roll must not stop
/// that agent on the old record. It waits for a record that answers its own
/// request, and the manifest carries the call parked for *this* roll.
#[test]
fn a_roll_after_a_stand_down_never_trusts_the_earlier_pauses_safe_point() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());

    // ---- Roll 1: raised, then aborted before anything was stopped. ----------
    let plan1 = begin(&drain, dir.path(), tuning(30_000));
    let host1 = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host1.cands[0].pause_dir.clone().unwrap();
    let run1 = spawn_h4(&drain, host1.clone(), &plan1, &ledger("rp-roll-1", &plan1));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert_eq!(drain.abort_checked(), AbortOutcome::Aborted);
    assert_eq!(run1.join().unwrap(), H4Outcome::StoodDown { promoted: false });
    assert!(host1.torn.lock().unwrap().is_empty());
    // The agent had parked for roll 1 and its hook never got to clean up.
    plant_safe_point(&pause_dir, Some("rp-roll-1"), "git push (the call parked for roll 1)");

    // ---- Roll 2: the same agent, the same item dir. --------------------------
    let plan2 = begin(&drain, dir.path(), tuning(20_000));
    let host2 = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let run2 = spawn_h4(&drain, host2.clone(), &plan2, &ledger("rp-roll-2", &plan2));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    // Raising the request removed the old record...
    assert!(roll_pause::read_safe_point(&pause_dir).is_none());
    // ...and one that turns up afterwards for the OLD request (a hook that
    // was slow to die) is ignored: the agent is mid-tool-call, not parked.
    plant_safe_point(&pause_dir, Some("rp-roll-1"), "git push (the call parked for roll 1)");
    std::thread::sleep(Duration::from_millis(400)); // twenty polls
    assert!(host2.torn.lock().unwrap().is_empty(), "stopped on a stale safe point");
    assert!(!drain.snapshot().pause.unwrap().stopped);

    // The agent now really parks for roll 2 (the real hook, one tool call).
    let hook = spawn_parking_agent(pause_dir.clone());
    assert!(matches!(run2.join().unwrap(), H4Outcome::Paused { .. }));
    hook.join().unwrap();

    assert_eq!(*host2.torn.lock().unwrap(), vec!["old".to_string()]);
    let m = load(&plan2);
    let old = item(&m, "old");
    assert_eq!(old.status, ItemStatus::Paused);
    let sp = old.safe_point.as_ref().unwrap();
    assert_eq!(
        sp.parked_summary.as_deref(),
        Some("cargo test"),
        "the call parked for THIS roll"
    );
}

// ============================================================================
// Finding 3: dispatches that are mid-spawn
// ============================================================================

/// The production representation: `begin_issue_dispatch` has claimed, flipped
/// the label and spawned the child, and `finish_issue_dispatch` has not yet
/// recorded the entry. H4 waits for it instead of snapshotting past it,
/// refuses a new dispatch meanwhile, and the agent ends up in the manifest
/// and stopped, not still running when the daemon exits.
#[test]
#[serial]
fn a_dispatch_that_is_mid_spawn_is_waited_for_and_ends_up_in_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::env::remove_var("LOOM_REPO");
    let (mut registry, _gh_log) = test_support::crash_before_finish_registry(&root);

    let begun = registry
        .begin_issue_dispatch(&SweepKind::Issue(6615), None, None, None, None, None)
        .unwrap();
    let BeginIssueDispatch::Spawned(mut prepared) = begun else {
        panic!("expected a spawned child");
    };
    let agent_pid = prepared.child.id();
    assert_eq!(registry.mid_spawn_dispatches(), 1);
    assert!(
        sweep_candidates_of(&registry, &root).is_empty(),
        "a registry snapshot taken now does not see the agent"
    );

    let host = Arc::new(RegistryHost {
        registry: Arc::new(Mutex::new(registry)),
        root: root.clone(),
        events: Mutex::default(),
        leases: Mutex::default(),
    });
    let drain = Arc::new(DrainState::new());
    let mut plan = begin(&drain, &root, tuning(10_000));
    plan.tuning.forge_floor = Duration::from_secs(20);
    let run = spawn_h4(&drain, host.clone(), &plan, &ledger("rp-mid-spawn", &plan));

    // H4 closes dispatch and waits in step 1.
    assert!(test_support::wait_for_condition(20_000, || {
        host.registry.lock().unwrap().closed_for_roll()
    }));
    std::thread::sleep(Duration::from_millis(300));
    let status = drain.snapshot().pause.unwrap();
    assert_eq!((status.step, status.items), (1, 0), "still waiting, nothing snapshotted");
    // Nothing new starts from the moment the pause is requested.
    let refused = host
        .registry
        .lock()
        .unwrap()
        .begin_issue_dispatch(&SweepKind::Issue(6616), None, None, None, None, None)
        .err()
        .expect("a new dispatch is refused while the roll is pausing");
    assert!(format!("{refused:#}").contains("pausing every agent"), "{refused:#}");

    // The dispatch finishes the way production does: polled off the lock,
    // then recorded under it.
    let (token, runtime, death) = poll_and_classify_spawned_child(
        &mut prepared.child,
        &prepared.log_path,
        &prepared.header_anchor,
    );
    host.registry
        .lock()
        .unwrap()
        .finish_issue_dispatch(*prepared, token, runtime, death)
        .unwrap();
    assert_eq!(host.registry.lock().unwrap().mid_spawn_dispatches(), 0);

    let H4Outcome::Paused { manifest_id, .. } = run.join().unwrap() else {
        panic!("expected Paused");
    };
    let m = load(&plan);
    assert_eq!(m.items.len(), 1, "{:?}", m.items);
    let it = &m.items[0];
    assert_eq!(it.issue, Some(6615));
    assert_eq!(it.pid, Some(agent_pid));
    assert_eq!(it.disposition, Disposition::Requeue);
    assert_eq!(it.reason.as_deref(), Some(pause_classify::REASON_YOUNG));
    assert!(it.stopped_at.is_some());
    assert!(
        test_support::wait_for_condition(10_000, || !teardown::pid_running(agent_pid)),
        "the mid-spawn agent is still running after H4"
    );
    roll_pause::hold::release(&it.id, &manifest_id);
}

fn sweep_candidates_of(
    registry: &crate::sweep_registry::SweepRegistry,
    root: &Path,
) -> Vec<Candidate> {
    host::sweep_candidates(registry, root)
}

/// The wait is bounded: a spawn that never completes does not hold the roll,
/// and the manifest says an agent was left out.
#[test]
fn a_stuck_spawn_does_not_hold_the_roll_past_the_settle_window() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(400));
    let host = Arc::new(FakeHost::default());
    host.pending.store(1, Ordering::SeqCst);

    let started = Instant::now();
    assert!(matches!(run_h4(&drain, host, &plan), H4Outcome::Paused { .. }));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(started.elapsed() >= plan.tuning.settle_window());
    assert!(load(&plan)
        .events
        .iter()
        .any(|e| e.event == "unsettled_dispatches" && e.detail.as_deref() == Some("1")));
}

/// Dispatch is closed for exactly as long as the pause needs it: reopened by
/// a stand-down, kept closed when the daemon is about to exit.
#[test]
fn dispatch_is_closed_during_the_pause_and_reopened_only_by_a_stand_down() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();
    let run = spawn_h4(&drain, host.clone(), &plan, &ledger("rp-gate", &plan));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert_eq!(host.closed.lock().unwrap().iter().collect::<Vec<_>>(), vec!["rp-gate"]);
    assert_eq!(drain.abort_checked(), AbortOutcome::Aborted);
    assert_eq!(run.join().unwrap(), H4Outcome::StoodDown { promoted: false });
    assert!(host.closed.lock().unwrap().is_empty(), "a stand-down reopens dispatch");

    let plan = begin(&drain, dir.path(), tuning(200));
    let host = Arc::new(FakeHost::default());
    assert!(matches!(run_h4(&drain, host.clone(), &plan), H4Outcome::Paused { .. }));
    assert_eq!(host.closed.lock().unwrap().len(), 1, "closed until the daemon exits");
}

// ============================================================================
// Finding 4: a pause that cannot finish
// ============================================================================

#[test]
fn the_default_h4_deadline_fits_the_documented_budget() {
    let t = PauseRollTuning::defaults(Duration::from_secs(900));
    assert_eq!(t.settle_window(), Duration::from_secs(30));
    assert_eq!(t.h4_deadline(), Duration::from_secs(270));
    // H4 at its deadline, then H5's probation and resume budget, is still
    // under the 600 s the validation allows against a 15-minute lease.
    let whole_roll = t.h4_deadline() + t.verify_probation + t.resume_budget;
    assert!(whole_roll < Duration::from_secs(600), "{whole_roll:?}");
}

/// A cand for a real process tree this test spawned.
fn real_cand(dir: &Path, id: &str, issue: u32, age_secs: i64, agent: &FakeAgent) -> Candidate {
    Candidate {
        pid: Some(agent.pid()),
        pgid: Some(agent.pid()),
        ..cand(dir, id, issue, age_secs)
    }
}

/// The committed side of the deadline. The teardown of the first agent hangs
/// (as a wedged process-table command would hang it), after the commit: an
/// abort is refused and nothing would ever restart the daemon. Ending the run
/// kills what is left by process group, writes the manifest with what is
/// known, and returns `Paused` so the caller restarts.
#[test]
fn a_committed_pause_that_hangs_is_finished_by_force_and_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut young = FakeAgent::spawn(dir.path(), "young");
    let mut old = FakeAgent::spawn(dir.path(), "old");
    let mut cands = vec![
        real_cand(dir.path(), "young", 2, 5, &young),
        real_cand(dir.path(), "old", 1, 900, &old),
    ];
    host::stamp_proc_starts(&mut cands);
    assert!(cands.iter().all(|c| c.proc_started_at.is_some()));

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (entered_in, release_in) = (entered.clone(), release.clone());
    let host = Arc::new(FakeHost {
        cands,
        // The stuck teardown: it never signals anything and never returns.
        on_teardown: Some(Box::new(move |_id| {
            entered_in.store(true, Ordering::SeqCst);
            while !release_in.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
        })),
        ..FakeHost::default()
    });
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(300));
    let ledger = ledger("rp-hung", &plan);
    let run = spawn_h4(&drain, host.clone(), &plan, &ledger);
    assert!(test_support::wait_for_condition(20_000, || entered.load(Ordering::SeqCst)));
    assert!(drain.snapshot().pause.unwrap().stopped, "the pause committed before the hang");
    assert!(matches!(drain.abort_checked(), AbortOutcome::Refused(_)));

    let outcome = end_unfinished(
        &drain,
        host.as_ref(),
        &plan,
        &ledger,
        "the pause did not finish within its 1s H4 deadline",
    );

    assert_eq!(
        outcome,
        H4Outcome::Paused {
            manifest_id: "rp-hung".to_string(),
            then_exit: false
        },
        "the caller restarts"
    );
    // Both trees' process groups were killed without the stuck teardown.
    for agent in [&mut young, &mut old] {
        assert!(
            test_support::wait_for_condition(10_000, || matches!(
                agent.child.try_wait(),
                Ok(Some(_))
            )),
            "an agent's group leader survived the forced finish"
        );
    }
    // The manifest records what is known, and is one the next start finishes.
    let m = raw_manifest(&plan);
    assert_eq!((m.manifest_id.as_str(), m.phase.clone()), ("rp-hung", Phase::Pausing));
    for id in ["young", "old"] {
        let it = item(&m, id);
        assert_eq!(
            (it.disposition.clone(), it.status.clone()),
            (Disposition::Requeue, ItemStatus::Planned),
            "{id}"
        );
        assert!(it.stopped_at.is_some(), "{id}");
    }
    assert_eq!(item(&m, "old").reason.as_deref(), Some(REASON_BUDGET_MISSED));
    assert!(m.events.iter().any(|e| e.event == "h4_forced"));
    assert!(host
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|(t, p)| t == "daemon.roll.forced" && p["forced_items"] == 2));

    // The stuck thread coming back later cannot overwrite the forced manifest.
    release.store(true, Ordering::SeqCst);
    assert!(matches!(run.join().unwrap(), H4Outcome::Paused { .. }));
    let after = raw_manifest(&plan);
    assert_eq!(after.phase, Phase::Pausing);
    assert_eq!(after.events.last().unwrap().event, "h4_forced");
}

/// The uncommitted side, and the failed-task case: nothing was stopped, so
/// the pause is ended and everything it left is removed. Without this the
/// agents stayed denied on every tool call and their sweeps never reaped.
#[test]
fn an_uncommitted_pause_that_fails_releases_its_requests_and_holds() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();
    let ledger = ledger("rp-failed", &plan);
    let run = spawn_h4(&drain, host.clone(), &plan, &ledger);
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert_eq!(host.held.lock().unwrap().len(), 1);
    assert!(plan.manifest_path.is_file());

    let outcome =
        end_unfinished(&drain, host.as_ref(), &plan, &ledger, "the pause task failed (panicked)");

    assert!(matches!(&outcome, H4Outcome::Failed(why) if why.contains("PAUSE FAILED")));
    assert!(!drain.is_draining(), "dispatch resumed");
    assert!(drain.snapshot().note.unwrap().contains("pause-failed"));
    assert!(!roll_pause::is_requested(&pause_dir), "the pause request is withdrawn");
    assert!(host.held.lock().unwrap().is_empty(), "the reaper hold is released");
    assert!(host.closed.lock().unwrap().is_empty(), "dispatch is reopened");
    assert!(!plan.manifest_path.exists(), "the manifest is deleted");
    assert!(host.torn.lock().unwrap().is_empty(), "nothing was stopped");
    assert!(host
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|(t, _)| t == "daemon.roll.pause_failed"));
    // The run itself, when it notices, has nothing left to undo.
    assert_eq!(run.join().unwrap(), H4Outcome::StoodDown { promoted: false });
}

// ============================================================================
// The supersede race, and the registry lock
// ============================================================================

/// A superseded run and its replacement share the manifest path, the item
/// dirs and the hold set. The old run's late stand-down removes only what it
/// created.
#[test]
fn a_late_stand_down_leaves_the_replacement_runs_manifest_requests_and_holds() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let c = cand(dir.path(), "old", 1, 900);
    let pause_dir = c.pause_dir.clone().unwrap();
    let host = FakeHost::default();
    let manifest = |id: &str| PauseManifest {
        manifest_id: id.to_string(),
        ..minimal_manifest()
    };
    let request = |id: &str| roll_pause::PauseRequest {
        requested_at: rfc3339(Utc::now()),
        manifest_id: Some(id.to_string()),
        ..roll_pause::PauseRequest::default()
    };

    // The old run got as far as its requests, then was superseded.
    let old = RunLedger::new("rp-old".to_string(), plan.manifest_path.clone());
    old.set_candidates(std::slice::from_ref(&c));
    old.save(&manifest("rp-old")).unwrap();
    host.hold(&c, true, "rp-old");
    roll_pause::request_pause(&pause_dir, &request("rp-old")).unwrap();

    // Its replacement is already at the same point...
    let new = RunLedger::new("rp-new".to_string(), plan.manifest_path.clone());
    new.set_candidates(std::slice::from_ref(&c));
    new.save(&manifest("rp-new")).unwrap();
    host.hold(&c, true, "rp-new");
    roll_pause::request_pause(&pause_dir, &request("rp-new")).unwrap();
    plant_safe_point(&pause_dir, Some("rp-new"), "the replacement's parked call");

    // ...when the old run finally stands down.
    old.undo(&host);

    assert_eq!(raw_manifest(&plan).manifest_id, "rp-new", "the replacement's manifest stands");
    assert_eq!(
        roll_pause::read_request(&pause_dir)
            .unwrap()
            .manifest_id
            .as_deref(),
        Some("rp-new"),
        "and its pause request"
    );
    assert!(roll_pause::read_safe_point(&pause_dir).is_some(), "and its safe point");
    assert_eq!(host.held.lock().unwrap().get("old").map(String::as_str), Some("rp-new"));

    // The replacement's own stand-down does remove them.
    new.undo(&host);
    assert!(!plan.manifest_path.exists());
    assert!(!roll_pause::is_requested(&pause_dir));
    assert!(roll_pause::read_safe_point(&pause_dir).is_none());
    assert!(host.held.lock().unwrap().is_empty());
}

fn minimal_manifest() -> PauseManifest {
    PauseManifest {
        schema_version: pause_manifest::SCHEMA_VERSION,
        manifest_id: String::new(),
        phase: Phase::Pausing,
        written_by: WrittenBy {
            version: "0.19.887".to_string(),
            artifact_sha256: None,
            pid: None,
            host: None,
            supervisor: None,
        },
        roll: Roll {
            from_version: None,
            to_version: "0.19.900".to_string(),
            to_artifact_sha256: None,
            target_source: None,
            staged_at: None,
            pause_started_at: Utc::now(),
            pause_completed_at: None,
            pause_duration_ms: None,
            pause_budget_secs: None,
            min_resumable_age_secs: None,
            max_age_secs: 900,
        },
        items: Vec::new(),
        events: Vec::new(),
    }
}

/// The requeue's `gh` calls run without the registry mutex: while a slow
/// forge write is in flight, the registry is free for everything else.
#[test]
fn a_requeue_does_not_hold_the_registry_mutex_across_its_forge_writes() {
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("slow-gh.sh");
    let started = dir.path().join("gh.started");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env bash\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then echo false; \
             exit 0; fi\nif [ \"$1\" = issue ] && [ \"$2\" = edit ]; then touch \"{}\"; sleep 2; \
             fi\nexit 0\n",
            started.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = crate::sweep_registry::SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(gh);
    let registry = Arc::new(Mutex::new(crate::sweep_registry::SweepRegistry::new(config)));
    let notice = RollRequeueNotice {
        from_version: "0.19.887".to_string(),
        reason: "young-agent-reset".to_string(),
        manifest_id: "rp-1".to_string(),
        ..RollRequeueNotice::default()
    };

    let worker = {
        let registry = registry.clone();
        std::thread::spawn(move || host::requeue_off_the_registry_lock(&registry, 7, &notice))
    };
    assert!(
        test_support::wait_for_condition(20_000, || started.is_file()),
        "the slow forge write never started"
    );
    assert!(registry.try_lock().is_ok(), "the registry mutex is held across a forge write");
    worker.join().unwrap().unwrap();
}

/// A forced finish with nothing left to force still writes a manifest the
/// loader accepts as an ordinary `pausing` one.
#[test]
fn a_forced_manifest_loads_like_any_pausing_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(200));
    let ledger = RunLedger::new("rp-forced".to_string(), plan.manifest_path.clone());
    ledger
        .save(&PauseManifest {
            manifest_id: "rp-forced".to_string(),
            ..minimal_manifest()
        })
        .unwrap();
    let (id, forced) = ledger.force_finish("test", "0.19.887");
    assert_eq!((id.as_str(), forced), ("rp-forced", 0));
    match pause_manifest::load(&plan.manifest_path, Utc::now()) {
        LoadOutcome::Loaded(m) => assert_eq!(m.phase, Phase::Pausing),
        other => panic!("the forced manifest must load: {other:?}"),
    }
}
