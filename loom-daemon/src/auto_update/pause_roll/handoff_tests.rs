//! H4 → H5 hand-off (#10832): the manifest the real H4 code writes is the
//! manifest the real H5 code reads. Each test runs PR 2's pause against real
//! process trees and a real registry, "restarts" (a fresh registry over the
//! same workspace), and runs the resume on what the pause left behind.

use super::ledger::RunLedger;
use super::supervise::end_unfinished;
use super::tests::{begin, item, register, spawn_parking_agent, tuning, FakeAgent, RegistryHost};
use super::*;
use crate::auto_update::pause_manifest::LoadOutcome;
use crate::auto_update::pause_resume::test_support::{
    arm_for, fake_gh, gh_calls, real_registry, RegistryHost as ResumeRegistryHost,
};
use crate::auto_update::pause_resume::{self, H5Outcome, ResumePlan, ResumeTuning};
use crate::sweep_registry::test_support;
use std::sync::Mutex;

fn resume_plan(plan: &PausePlan) -> ResumePlan {
    ResumePlan {
        manifest_path: plan.manifest_path.clone(),
        // The binary the roll was going to.
        running_version: plan.target.to_version.clone().unwrap(),
        tuning: ResumeTuning {
            verify_probation: Duration::from_millis(50),
            resume_budget: Duration::from_secs(30),
            probe_interval: Duration::from_millis(10),
            start_confirm: Duration::from_millis(50),
            poll: Duration::from_millis(10),
            forge_window: Duration::from_secs(20),
        },
    }
}

fn archived(plan: &PausePlan, id: &str) -> PauseManifest {
    let path = plan
        .manifest_path
        .parent()
        .unwrap()
        .join(format!("roll-pause-manifest.{id}.done.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The whole cycle on one host: H4 pauses one agent at a safe point, resets a
/// young one and requeues one that misses the budget; the daemon "restarts";
/// H5 reads that manifest (its request id, its events, its observed process
/// start times), resumes the paused agent from its session with the call it
/// was parked on named in the resume prompt, releases the two H4 requeued,
/// and archives the manifest `resumed`.
#[test]
fn h5_resumes_and_releases_from_the_manifest_h4_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (gh, gh_log) = fake_gh(&root, "loom:building");
    // The resumed session records what it was launched with, then stays up.
    let record = root.join("resumed.env");
    let spawn_body = format!(
        "printf 'resume=%s\\nprompt=%s\\nitem=%s\\ndone\\n' \"$LOOM_RESUME_SESSION_ID\" \
         \"$LOOM_RESUME_PROMPT\" \"$LOOM_DAEMON_ITEM_ID\" >> \"{}\"\nsleep 300",
        record.display()
    );
    let mut registry = real_registry(&root, Some(gh.clone()), &spawn_body);
    let mut paused = FakeAgent::spawn(&root, "paused");
    let mut young = FakeAgent::spawn(&root, "young");
    let mut missed = FakeAgent::spawn(&root, "missed");
    let (paused_id, paused_dir) = register(&mut registry, &root, &paused, 101, 900);
    let (young_id, _) = register(&mut registry, &root, &young, 102, 5);
    let (missed_id, _) = register(&mut registry, &root, &missed, 103, 900);
    let hook = spawn_parking_agent(paused_dir);

    // ---- H4, PR 2's code ------------------------------------------------------
    let pause_host = Arc::new(RegistryHost {
        registry: Arc::new(Mutex::new(registry)),
        root: root.clone(),
        events: Mutex::default(),
        leases: Mutex::default(),
    });
    let drain = DrainState::new();
    let mut plan = begin(&drain, &root, tuning(6000));
    plan.tuning.forge_floor = Duration::from_secs(20);
    let H4Outcome::Paused { manifest_id, .. } = run_h4(&drain, pause_host.clone(), &plan) else {
        panic!("H4 must end Paused");
    };
    assert!(paused.gone() && young.gone() && missed.gone());
    hook.join().unwrap();
    let written = match pause_manifest::load(&plan.manifest_path, Utc::now()) {
        LoadOutcome::Loaded(m) => m,
        other => panic!("H5 must be able to load what H4 wrote: {other:?}"),
    };
    assert_eq!(
        (written.manifest_id.as_str(), written.phase.clone()),
        (manifest_id.as_str(), Phase::Paused)
    );
    let p = item(&written, &paused_id);
    assert!(p.pid_started_at.is_some(), "the observed process start is recorded");
    let session = p
        .resume_handle
        .as_ref()
        .unwrap()
        .session_id
        .clone()
        .unwrap();

    // ---- The restart: this process's in-memory state is gone -------------------
    for id in [&paused_id, &young_id, &missed_id] {
        roll_pause::hold::release(id, &manifest_id);
    }
    drop(pause_host);
    let mut registry = real_registry(&root, Some(gh), &spawn_body);
    arm_for(&root, &written);
    assert_eq!(registry.reconstruct().unwrap(), 0, "recovery leaves the manifest's items alone");
    let before = gh_calls(&gh_log);

    // ---- H5 --------------------------------------------------------------------
    let resume_host = ResumeRegistryHost::new(registry);
    let H5Outcome::Finished(status) =
        pause_resume::run_h5(resume_host.clone(), &resume_plan(&plan))
    else {
        panic!("H5 must finish");
    };

    assert_eq!(status.resumed, 1);
    assert_eq!(status.requeued_by_reason["young-agent-reset"], 1);
    assert_eq!(status.requeued_by_reason[REASON_BUDGET_MISSED], 1);
    assert_eq!(status.resumed_on.as_deref(), Some("target"));
    assert_eq!(*resume_host.launches.lock().unwrap(), vec![paused_id.clone()]);
    // The resumed session: the same session id, and a prompt that names the
    // call the pause parked.
    let recorded = test_support::assert_child_wrote(&record, "done");
    assert!(recorded.contains(&format!("resume={session}")), "{recorded}");
    assert!(
        recorded.contains("Bash: cargo test") && recorded.contains("did NOT run"),
        "{recorded}"
    );
    let lock = |issue: u32| root.join(format!(".loom/locks/issue-{issue}/owner.json"));
    let owner: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lock(101)).unwrap()).unwrap();
    assert_eq!(owner["resume_handle"]["resume_of"], paused_id);
    assert!(!lock(102).exists() && !lock(103).exists(), "H4's requeued items are released");
    // H4 already did the requeue forge writes; H5 adds none.
    let writes = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("issue edit") || l.starts_with("issue comment"))
            .count()
    };
    assert_eq!(writes(&gh_calls(&gh_log)), writes(&before));
    // The archive keeps H4's audit trail and adds H5's.
    let done = archived(&plan, &manifest_id);
    assert_eq!(done.phase, Phase::Resumed);
    assert_eq!(item(&done, &paused_id).status, ItemStatus::Resumed);
    for event in [
        "pause_started",
        "safe_point",
        "pause_completed",
        "resume_started",
        "resumed",
    ] {
        assert!(done.events.iter().any(|e| e.event == event), "missing `{event}`");
    }
    assert!(!roll_pause::suppress::is_armed_for(&manifest_id));
    let new_id = owner["sweep_id"].as_str().unwrap().to_string();
    resume_host
        .registry
        .lock()
        .unwrap()
        .abandon_roll_resume(&new_id);
}

/// A pause H4 had to finish by force leaves `phase = pausing` with `forced`
/// / `h4_forced` events and its remaining items `requeue` / `planned`. H5
/// reads that manifest and finishes it: every item ends requeued, nothing is
/// resumed, and the forced events survive into the archive.
#[test]
fn h5_finishes_a_manifest_h4_wrote_by_force() {
    use super::tests::{cand, FakeHost};
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut young = FakeAgent::spawn(&root, "young");
    let mut old = FakeAgent::spawn(&root, "old");
    let real = |id: &str, issue: u32, age: i64, agent: &FakeAgent| host::Candidate {
        pid: Some(agent.pid()),
        pgid: Some(agent.pid()),
        ..cand(&root, id, issue, age)
    };
    let mut cands = vec![real("young", 2, 5, &young), real("old", 1, 900, &old)];
    host::stamp_proc_starts(&mut cands);
    let (entered, release) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (entered_in, release_in) = (entered.clone(), release.clone());
    let pause_host = Arc::new(FakeHost {
        cands,
        on_teardown: Some(Box::new(move |_id| {
            entered_in.store(true, Ordering::SeqCst);
            while !release_in.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
        })),
        ..FakeHost::default()
    });
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, &root, tuning(300));
    let ledger =
        Arc::new(RunLedger::new("rp-forced-handoff".to_string(), plan.manifest_path.clone()));
    let run = {
        let (d, h, p, l) = (drain.clone(), pause_host.clone(), plan.clone(), ledger.clone());
        std::thread::spawn(move || run_h4_with(&d, h, &p, &l))
    };
    assert!(test_support::wait_for_condition(20_000, || entered.load(Ordering::SeqCst)));
    let outcome =
        end_unfinished(&drain, pause_host.as_ref(), &plan, &ledger, "the H4 deadline passed");
    assert!(matches!(outcome, H4Outcome::Paused { .. }));
    release.store(true, Ordering::SeqCst);
    let _ = run.join();
    for agent in [&mut young, &mut old] {
        assert!(test_support::wait_for_condition(10_000, || matches!(
            agent.child.try_wait(),
            Ok(Some(_))
        )));
    }

    let resume_host = ResumeRegistryHost::new(real_registry(&root, None, "sleep 300"));
    let H5Outcome::Finished(status) =
        pause_resume::run_h5(resume_host.clone(), &resume_plan(&plan))
    else {
        panic!("H5 must finish a forced manifest");
    };

    assert_eq!(status.resumed, 0);
    assert_eq!(
        status.requeued_by_reason.values().sum::<u32>(),
        2,
        "{:?}",
        status.requeued_by_reason
    );
    assert!(resume_host.launches.lock().unwrap().is_empty());
    let done = archived(&plan, "rp-forced-handoff");
    assert_eq!(done.phase, Phase::Resumed);
    assert!(done.events.iter().any(|e| e.event == "h4_forced"));
    assert!(done.items.iter().all(|i| i.status == ItemStatus::Requeued), "{:?}", done.items);
}
