//! H5 against real sweep registries, real processes and the real recovery
//! passes, behind a fake `gh` (#10832).

use super::fake_host::FakeHost;
use super::*;
use crate::auto_update::pause_resume::test_support::{
    arm_for, fake_gh, gh_calls, real_registry, RegistryHost,
};

/// Rollback safety, end to end through H3: the old binary comes back, runs
/// H5 on the manifest of the roll that did not take, and the very next roll
/// trigger for that same target is refused with nothing paused. No refetch
/// loop that pauses the host again and again.
#[tokio::test]
#[serial_test::serial(loom_daemon_supervisor)]
async fn the_old_binary_does_not_pause_again_for_the_target_that_just_failed() {
    use crate::auto_update::pause_roll::{start_pause_roll, RollTarget};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
    std::env::set_var(crate::auto_update::AUTO_UPDATE_STATE_DIR_ENV, root.join("state"));
    let mut plan = plan(&root);
    plan.running_version = "0.19.887".to_string();
    assert_eq!(Some(plan.manifest_path.clone()), pause_manifest::manifest_path());
    write(&plan, &manifest("rp-loop", Phase::Paused, vec![sweep(&root, "s-1", 1)]));
    let resumed = tokio::task::spawn_blocking({
        let plan = plan.clone();
        move || finished(run_h5(Arc::new(FakeHost::default()), &plan))
    })
    .await
    .unwrap();
    assert_eq!(resumed.resumed_on.as_deref(), Some("rollback"));

    let bus = Arc::new(crate::event_bus::EventBus::new());
    let mut events = bus.subscribe(["daemon.roll.refused"]);
    let pool = Arc::new(crate::workspace_pool::WorkspacePool::new(
        bus.clone(),
        tokio::runtime::Handle::current(),
    ));
    let drain = Arc::new(DrainState::new());
    let target = RollTarget {
        source: pause_manifest::TargetSource::Floor,
        to_version: Some(RUNNING.to_string()),
        to_artifact_sha256: Some("abcd".to_string()),
        label: Some(format!("v{RUNNING}@abcd")),
    };

    let started = start_pause_roll(&drain, &pool, &root, &bus, &target);

    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    std::env::remove_var(crate::auto_update::AUTO_UPDATE_STATE_DIR_ENV);
    assert!(!started, "the failed target is held back");
    assert!(!drain.is_draining(), "nothing was paused");
    assert_eq!(drain.generation(), 0);
    assert!(!plan.manifest_path.exists(), "and no new manifest was written");
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("a daemon.roll.refused event is published")
        .unwrap();
    let refused = serde_json::to_value(&event).unwrap().to_string();
    assert!(
        refused.contains("roll-attempt-backoff") && refused.contains(RUNNING),
        "{refused}"
    );
}

// ============================================================================
// Integration: real registries, real processes, the real recovery passes
// ============================================================================

/// Leave `issue` as H4 leaves a paused sweep and return its manifest item.
fn paused_on_disk(
    reg: &SweepRegistry,
    root: &Path,
    issue: u32,
    checkpoint: Option<&str>,
) -> ManifestItem {
    let id = format!("sweep-issue-{issue}-1000");
    test_support::write_lock_owner(reg, issue, &id, 999_000 + issue);
    let session =
        crate::sweep_registry::resume_handle::DispatchSession::new(&id, root, Some("claude"))
            .unwrap();
    reg.stamp_resume_handle_in_lock(issue, &session, Some("opus"), None);
    if let Some(phase) = checkpoint {
        test_support::write_checkpoint(reg, issue, phase);
    }
    // Appended, not replaced: every paused sweep keeps its journal entry.
    let journal_path = root.join("sweeps.json");
    let mut journal = crate::sweep_journal::load(&journal_path);
    journal.entries.push(crate::sweep_journal::JournalEntry {
        repo: root.display().to_string(),
        issue,
        pid: 999_000 + issue,
        started_at: Utc::now(),
    });
    crate::sweep_journal::save(&journal_path, &journal).unwrap();
    let mut it = sweep(root, &id, issue);
    it.checkpoint_phase = checkpoint.map(str::to_string);
    it.resume_handle.as_mut().unwrap().session_id = session.claude_session_id;
    it
}

fn lock_file(root: &Path, issue: u32) -> PathBuf {
    root.join(format!(".loom/locks/issue-{issue}/owner.json"))
}

/// AC, end to end over a real registry: while the manifest is live, restart
/// recovery (`reconstruct`), the reaper, the live-claim probe behind claim
/// reconciliation and a fresh dispatch are all blocked for its items; H5 then
/// resumes one in the same workspace as a new process with the claim kept and
/// `resume_of` lineage, and requeues one whose resume does not start; at the
/// end the block is lifted (both edges).
#[test]
fn a_live_manifest_blocks_recovery_until_h5_resumes_or_requeues_each_item() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let plan = plan(&root);
    let (gh, gh_log) = fake_gh(&root, "loom:building");
    // The resumed session stays up for issue 101 and dies at once for 102.
    let mut reg = real_registry(
        &root,
        Some(gh),
        "case \"$LOOM_SWEEP_ID\" in *issue-102-*) exit 3 ;; esac\nsleep 300",
    );
    let ok = paused_on_disk(&reg, &root, 101, Some("builder"));
    let dies = paused_on_disk(&reg, &root, 102, Some("builder"));
    let m = manifest("rp-live", Phase::Paused, vec![ok.clone(), dies.clone()]);
    write(&plan, &m);

    // ---- Edge 1: armed before the first recovery pass ----------------------
    arm_for(&root, &m);
    assert!(!roll_pause::suppress::host_verified());
    assert_eq!(reg.reconstruct().unwrap(), 0, "no Crashed entry for a paused item");
    assert!(
        lock_file(&root, 101).is_file() && lock_file(&root, 102).is_file(),
        "the stale locks stay"
    );
    assert_eq!(reg.reap_once(), 0);
    assert!(lock_file(&root, 101).is_file());
    // Claim reconciliation's reclaim is vetoed by the live-claim probe, the
    // orphan-process and worktree reapers see the worktree as owned, and a
    // fresh dispatch of the issue is refused.
    let evidence = crate::live_claim::probe(&root, None, 101).expect("held by the manifest");
    assert!(matches!(evidence, crate::live_claim::LiveClaimEvidence::PausedForRoll { .. }));
    assert!(crate::worktree_ops::liveness::active_spawn_loop_issues(&root).contains(&101));
    // (`live_claim_evidence` is dispatch guard 2.9's own probe.)
    let guard = reg
        .live_claim_evidence(101)
        .expect("a fresh dispatch of #101 is refused");
    assert!(guard.to_string().contains("paused for a daemon roll"), "{guard}");
    assert!(lock_file(&root, 101).is_file());

    // ---- H5 ------------------------------------------------------------------
    let host = RegistryHost::new(reg);
    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(*host.launches.lock().unwrap(), vec![ok.id.clone(), dies.id.clone()]);
    assert_eq!(
        (status.resumed, &status.requeued_by_reason),
        (1, &BTreeMap::from([("session-resume-failed".to_string(), 1)]))
    );
    {
        // 101: a new running process, same claim, lineage recorded.
        let mut reg = host.registry.lock().unwrap();
        let raw = std::fs::read_to_string(lock_file(&root, 101)).unwrap();
        let owner: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let new_id = owner["sweep_id"].as_str().unwrap().to_string();
        assert_ne!(new_id, ok.id);
        assert_eq!(owner["resume_handle"]["resume_of"], ok.id);
        assert_eq!(
            owner["resume_handle"]["session_id"],
            ok.resume_handle
                .as_ref()
                .unwrap()
                .session_id
                .clone()
                .unwrap()
        );
        let entry = reg.get(&new_id).expect("the resumed sweep is tracked");
        assert_eq!(entry.state, crate::types::SweepState::Running);
        assert!(crate::sweep_registry::is_pid_alive(entry.pid));
        assert!(!roll_pause::hold::is_held(&new_id), "the reaper supervises it from here");
        // 102: its resume died, so it was requeued and released.
        assert!(!lock_file(&root, 102).exists(), "the requeued item's lock is released");
        assert!(
            root.join(".loom/sweep-checkpoint/issue-102.json").is_file(),
            "its checkpoint is kept"
        );
        assert!(reg
            .list(None)
            .iter()
            .all(|i| i.kind != crate::types::SweepKind::Issue(102)));
        reg.abandon_roll_resume(&new_id);
    }
    let calls = gh_calls(&gh_log);
    assert!(
        !calls.lines().any(|l| l.starts_with("issue edit 101")),
        "the kept claim is never re-flipped: {calls}"
    );
    assert_eq!(
        calls
            .lines()
            .filter(|l| l.starts_with("issue comment 102 "))
            .count(),
        1,
        "{calls}"
    );
    assert!(calls.contains("`session-resume-failed`"), "{calls}");
    assert!(calls.contains("could not resume it afterwards"), "{calls}");
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("issue edit 102") && l.contains("loom:issue")),
        "{calls}"
    );

    // ---- Edge 2: the block is lifted at the end of H5 -------------------------
    assert!(!roll_pause::suppress::is_armed_for("rp-live"));
    assert_eq!(roll_pause::suppress::held_issue(&root, 101), None);
    assert!(!host.drain.is_draining());
    assert_eq!(archived(&plan, "rp-live").phase, Phase::Resumed);
}

/// AC, rollback to a binary older than #10715 (a double of its recovery
/// passes: the real `reconstruct`, reaper and claim-reconciliation decision,
/// run with no knowledge of the manifest). From a paused host every claim is
/// recovered and none is stranded:
///
/// - a sweep with a checkpoint: `reconstruct` drops its stale lock and turns
///   the checkpoint into a `Crashed` entry, which the next dispatch resumes
///   from;
/// - every sweep, with or without a checkpoint: claim reconciliation finds
///   the dead journal pid and decides `Reclaim(DeadPid)`, held back by the
///   lease H4 refreshed for at most one TTL;
/// - a role run leaves nothing the daemon tracks (its own staleness rule).
///
/// A later manifest-aware start then finds the claims already handled and
/// writes nothing for them: no double dispatch, manifest archived
/// `abandoned`.
#[test]
fn a_pre_10715_binary_recovers_every_claim_and_a_later_start_does_not_double_dispatch() {
    use crate::claim_reconciliation::{
        classify_lease_evidence, plan as plan_reclaims, BuildingIssue, LeaseEvidence,
        ReclaimReason, ReconcileAction,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let plan = plan(&root);
    let (gh, gh_log) = fake_gh(&root, "loom:building");
    let mut reg = real_registry(&root, Some(gh), "sleep 300");
    // What H4 leaves: locks, journal entries and (for one) a checkpoint, all
    // with dead pids.
    let with_checkpoint = paused_on_disk(&reg, &root, 201, Some("builder"));
    let no_checkpoint = paused_on_disk(&reg, &root, 202, None);
    let m = manifest(
        "rp-old",
        Phase::Paused,
        vec![
            with_checkpoint.clone(),
            no_checkpoint.clone(),
            role_run(&root, "r-judge", "judge"),
        ],
    );
    write(&plan, &m);

    // ---- The old binary starts. It has never heard of the manifest. ---------
    assert_eq!(reg.reconstruct().unwrap(), 1, "the checkpoint sweep becomes a Crashed entry");
    assert!(
        !lock_file(&root, 201).exists() && !lock_file(&root, 202).exists(),
        "stale locks dropped"
    );
    reg.reap_once();
    let crashed: Vec<_> = reg
        .list(None)
        .into_iter()
        .filter(|i| matches!(i.state, crate::types::SweepState::Crashed { .. }))
        .collect();
    assert_eq!(crashed.len(), 1);
    assert_eq!(crashed[0].kind, crate::types::SweepKind::Issue(201));
    assert_eq!(
        crashed[0].latest_phase.as_deref(),
        Some("builder"),
        "resumable from its checkpoint"
    );
    // Claim reconciliation sees both dead journal pids and decides to reclaim;
    // the lease H4 refreshed holds that back for one TTL, and no longer.
    let journal = crate::sweep_journal::load(&root.join("sweeps.json"));
    let now = Utc::now();
    let building = |number| BuildingIssue {
        number,
        updated_at: Some(now),
    };
    let decisions = plan_reclaims(
        &root.display().to_string(),
        &[building(201), building(202)],
        &journal,
        &|_| None,
        &|_| None,
        30.0,
        &crate::sweep_registry::is_pid_alive,
        6.0,
        now,
        false,
    );
    assert_eq!(
        decisions,
        vec![
            (201, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 999_201 })),
            (202, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 999_202 })),
        ],
        "no claim is stranded as loom:building"
    );
    let refreshed_at_h4 = now - chrono::Duration::minutes(1);
    assert!(matches!(
        classify_lease_evidence(Some(refreshed_at_h4), now, 15.0),
        LeaseEvidence::Fresh { .. }
    ));
    let one_ttl_later = now + chrono::Duration::minutes(15);
    assert!(
        !matches!(
            classify_lease_evidence(Some(refreshed_at_h4), one_ttl_later, 15.0),
            LeaseEvidence::Fresh { .. }
        ),
        "the reclaim proceeds at most one TTL after H4's refresh"
    );
    // (The reclaim itself: label restored, journal entry dropped.)
    for issue in [201, 202] {
        crate::sweep_journal::remove_sweep_at(
            &root.join("sweeps.json"),
            &root.display().to_string(),
            issue,
        )
        .unwrap();
    }

    // ---- Later, a manifest-aware binary starts ---------------------------------
    let writes = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("issue edit") || l.starts_with("issue comment"))
            .count()
    };
    let before = gh_calls(&gh_log);
    // Past the TTL (the reclaim above needed it): the manifest is stale.
    let mut stale = m.clone();
    stale.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(1000);
    write(&plan, &stale);
    arm_for(&root, &stale);
    let host = RegistryHost::new(reg);
    let status = finished(run_h5(host.clone(), &plan));

    assert!(host.launches.lock().unwrap().is_empty(), "nothing is dispatched a second time");
    assert_eq!(status.resumed, 0);
    assert_eq!(status.requeued_by_reason[REASON_STALE], 3);
    assert_eq!(archived(&plan, "rp-old").phase, Phase::Abandoned);
    let after = gh_calls(&gh_log);
    assert_eq!(
        writes(&after),
        writes(&before),
        "the claims were already recovered: H5 re-checks and writes nothing for them\n{after}"
    );
    assert!(!roll_pause::suppress::is_armed_for("rp-old"));
}

/// The same later start, but inside the lease TTL (the manifest still loads):
/// each resume re-checks the live claim first and finds it gone.
#[test]
fn a_later_start_inside_the_ttl_re_checks_the_claim_before_resuming() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let plan = plan(&root);
    let (gh, gh_log) = fake_gh(&root, "loom:building");
    let mut reg = real_registry(&root, Some(gh), "sleep 300");
    let it = paused_on_disk(&reg, &root, 301, Some("curated"));
    let m = manifest("rp-ttl", Phase::Paused, vec![it]);
    write(&plan, &m);
    // An older binary's restart recovery already took the claim.
    reg.reconstruct().unwrap();
    reg.reap_once();
    let before = gh_calls(&gh_log);

    arm_for(&root, &m);
    let host = RegistryHost::new(reg);
    let status = finished(run_h5(host.clone(), &plan));

    assert!(host.launches.lock().unwrap().is_empty(), "the claim lock is gone: no resume");
    assert_eq!(status.requeued_by_reason["guard-refused:claim-lock"], 1);
    let after = gh_calls(&gh_log);
    let writes = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("issue edit") || l.starts_with("issue comment"))
            .count()
    };
    assert_eq!(writes(&after), writes(&before), "nothing more is written to the forge: {after}");
}

/// The session-store and session-container checks map to their §9 reasons.
#[test]
fn a_missing_session_store_or_a_down_container_refuses_the_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_string_lossy().into_owned();
    let mut launch = RollResumeLaunch {
        runtime: "claude".to_string(),
        session_id: SID.to_string(),
        session_store: Some(store.clone()),
        ..RollResumeLaunch::default()
    };
    let up = |_: &str, _: &str| Ok(());
    let down = |name: &str, _: &str| Err(format!("session container {name} is not running"));
    let missing = host::session_checks(&launch, "/r", up).unwrap_err();
    assert_eq!(missing.reason, REASON_STORE, "{missing}");
    let project = tmp.path().join("projects").join("-r");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(format!("{SID}.jsonl")), "{}").unwrap();
    host::session_checks(&launch, "/r", up).unwrap();
    // A session-exec item also needs its container up with the cwd mounted.
    launch.container = Some("loom-codex-session-agent-3".to_string());
    let refused = host::session_checks(&launch, "/r", down).unwrap_err();
    assert_eq!(refused.reason, REASON_SESSION_DOWN, "{refused}");
    host::session_checks(&launch, "/r", up).unwrap();
}

/// A resumed sweep that exits non-zero is `session-resume-failed`; one whose
/// log carries a session-exec refusal is `session-down`; a clean exit is a
/// finished run; a sandbox other than the recorded one is refused.
#[test]
fn a_resumed_sweeps_liveness_is_read_from_its_child_and_its_own_log_region() {
    use crate::sweep_registry::roll_resume::ResumeChild;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("sweep.log");
    let launched = Launched {
        item_id: "new".to_string(),
        pid: Some(1),
        at: Instant::now(),
        log_path: Some(log.clone()),
        header_anchor: Some("sweep_id=new".to_string()),
    };
    let mut it = sweep(dir.path(), "old", 1);
    std::fs::write(&log, "==== dispatch sweep_id=new ====\n").unwrap();
    assert_eq!(host::sweep_liveness(ResumeChild::Running, &launched, &it), Liveness::Running);
    assert_eq!(
        host::sweep_liveness(ResumeChild::Exited(Some(0)), &launched, &it),
        Liveness::Finished
    );
    let Liveness::Died { reason, .. } =
        host::sweep_liveness(ResumeChild::Exited(Some(1)), &launched, &it)
    else {
        panic!("a non-zero exit is a failed resume");
    };
    assert_eq!(reason, "session-resume-failed");

    std::fs::write(
        &log,
        "==== dispatch sweep_id=new ====\n# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN\n",
    )
    .unwrap();
    let Liveness::Died { reason, .. } =
        host::sweep_liveness(ResumeChild::Exited(Some(78)), &launched, &it)
    else {
        panic!("a refused container is a failed resume");
    };
    assert_eq!(reason, REASON_SESSION_DOWN);

    std::fs::write(
        &log,
        "==== dispatch sweep_id=new ====\n[INFO] spawn-codex: sandbox=danger-full-access source=x\n",
    )
    .unwrap();
    it.resume_handle.as_mut().unwrap().sandbox = Some("workspace-write".to_string());
    let Liveness::Died { detail, .. } = host::sweep_liveness(ResumeChild::Running, &launched, &it)
    else {
        panic!("a sandbox other than the recorded one must not keep running");
    };
    assert!(
        detail.contains("danger-full-access") && detail.contains("workspace-write"),
        "{detail}"
    );
}

/// Teardown reach at H5 (PR 2's rule): the residue of an item is its recorded
/// pid and group, identity-checked, and its scope. Never its worktree.
#[test]
fn the_residue_spec_is_the_recorded_identity_and_never_the_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let mut it = sweep(dir.path(), "s-1", 1);
    it.scope_unit = Some("loom-agent-s-1.scope".to_string());
    it.worktree = Some(WorktreeRecord {
        path: "/r/.loom/worktrees/issue-1".to_string(),
        branch: None,
        head: None,
        dirty: None,
    });
    let observed = Utc::now() - chrono::Duration::seconds(42);
    it.pid_started_at = Some(rfc3339(observed));
    let spec = host::residue_spec(&it);
    assert_eq!((spec.pid, spec.pgid), (it.pid, it.pgid));
    assert_eq!(spec.pid_started_at.map(rfc3339), Some(rfc3339(observed)));
    assert_eq!(spec.recorded_started_at, it.run_started_at);
    assert_eq!(spec.scope_unit.as_deref(), Some("loom-agent-s-1.scope"));
    // `TreeSpec` has no worktree seed at all: nothing to assert but its shape.
    let crate::auto_update::pause_roll::teardown::TreeSpec {
        pid: _,
        pid_started_at: _,
        recorded_started_at: _,
        pgid: _,
        scope_unit: _,
    } = spec;
}

/// A long-lived process in its own group, as an agent's leader is.
fn live_process() -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("300").process_group(0);
    cmd.spawn().unwrap()
}

/// A forced H4 finish records a kill it never verified (`requeue`, `planned`,
/// `pause-budget-missed`, `stopped_at` set, `phase = pausing`). H5 must not
/// take that on trust. An agent that is in fact still alive, and still the
/// recorded process, is stopped before its claim is released; a live process
/// that merely wears the recorded pid number (another start time) is never
/// signalled.
#[test]
fn an_unverified_forced_kill_is_re_checked_by_pid_and_start_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let plan = plan(&root);
    let mut survivor = live_process();
    let mut stranger = live_process();
    let starts =
        crate::auto_update::pause_roll::teardown::observe_starts(&[survivor.id(), stranger.id()]);
    let forced = |id: &str, issue: u32, child: &std::process::Child, started: DateTime<Utc>| {
        let mut it = sweep(&root, id, issue);
        it.disposition = Disposition::Requeue;
        it.status = ItemStatus::Planned;
        it.reason = Some(REASON_BUDGET_MISSED.to_string());
        it.safe_point = None;
        it.pid = Some(child.id());
        it.pgid = Some(child.id());
        it.pid_started_at = Some(rfc3339(started));
        it.run_started_at = Some(started);
        it
    };
    let really_ours = forced("survived", 401, &survivor, starts[&survivor.id()]);
    // The same pid number, but the manifest recorded a process that started
    // an hour before this one: the number was recycled.
    let recycled =
        forced("recycled", 402, &stranger, starts[&stranger.id()] - chrono::Duration::hours(1));
    let mut m = manifest("rp-forced", Phase::Pausing, vec![really_ours, recycled]);
    m.roll.pause_completed_at = None;
    m.events.push(ManifestEvent {
        at: rfc3339(Utc::now()),
        by_version: Some("0.19.887".to_string()),
        item: None,
        event: "h4_forced".to_string(),
        detail: Some("the pause did not finish within its H4 deadline".to_string()),
    });
    write(&plan, &m);
    let host = RegistryHost::new(real_registry(&root, None, "sleep 300"));

    let status = finished(run_h5(host.clone(), &plan));

    assert!(
        test_support::wait_for_condition(20_000, || matches!(survivor.try_wait(), Ok(Some(_)))),
        "the agent the forced kill missed is still running"
    );
    assert!(matches!(stranger.try_wait(), Ok(None)), "a recycled pid number was signalled");
    assert_eq!(status.residue_reaped, 1);
    assert_eq!(status.requeued_by_reason[REASON_BUDGET_MISSED], 2);
    let done = archived(&plan, "rp-forced");
    assert_eq!(done.phase, Phase::Resumed);
    assert!(done.events.iter().any(|e| e.event == "h4_forced"), "H4's events are kept");
    assert!(done
        .events
        .iter()
        .any(|e| e.event == "residue_reaped" && e.item.as_deref() == Some("survived")));
    assert!(!done
        .events
        .iter()
        .any(|e| e.event == "residue_reaped" && e.item.as_deref() == Some("recycled")));
    let _ = stranger.kill();
    let _ = stranger.wait();
}
