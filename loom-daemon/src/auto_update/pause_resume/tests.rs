//! H5 tests (#10832). No live agents: a scripted host drives every decision,
//! and the integration tests drive real sweep registries, real process trees
//! and the real recovery passes behind a fake `gh`.

use super::host::{self, Launched, Liveness, ResumeHost};
use super::*;
use crate::auto_update::pause_manifest::{ResumeHandle, Roll, WorktreeRecord, WrittenBy};
use crate::auto_update::pause_roll::teardown::TeardownReport;
use crate::ipc::{DrainState, ResumeHold};
use crate::roll_pause::{self, PauseRequest};
use crate::sweep_registry::test_support;
use crate::sweep_registry::SweepRegistry;
use std::collections::BTreeSet;
use std::sync::Mutex;

const SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";
const RUNNING: &str = "0.19.900";

fn tuning() -> ResumeTuning {
    ResumeTuning {
        verify_probation: Duration::from_millis(60),
        resume_budget: Duration::from_secs(20),
        probe_interval: Duration::from_millis(10),
        start_confirm: Duration::from_millis(40),
        poll: Duration::from_millis(10),
        forge_window: Duration::from_secs(10),
    }
}

fn plan(dir: &Path) -> ResumePlan {
    ResumePlan {
        manifest_path: dir.join("state").join(pause_manifest::MANIFEST_FILE),
        running_version: RUNNING.to_string(),
        tuning: tuning(),
        armed: None,
    }
}

fn handle(runtime: Runtime) -> ResumeHandle {
    ResumeHandle {
        runtime,
        session_id: Some(SID.to_string()),
        session_store: None,
        account: None,
        model: Some("opus".to_string()),
        effort: None,
        cwd: None,
        container: None,
        sandbox: None,
        resume_count: 0,
        resume_of: None,
        lease_sweep_id: None,
    }
}

/// A paused, resumable sweep item.
fn sweep(dir: &Path, id: &str, issue: u32) -> ManifestItem {
    ManifestItem {
        id: id.to_string(),
        kind: ItemKind::Sweep,
        repo: dir.display().to_string(),
        disposition: Disposition::Resume,
        status: ItemStatus::Paused,
        reason: None,
        issue: Some(issue),
        pr: None,
        pid: Some(999_000 + issue),
        pid_started_at: None,
        pgid: Some(999_000 + issue),
        scope_unit: None,
        agent_started_at: Some(Utc::now() - chrono::Duration::seconds(900)),
        run_started_at: Some(Utc::now() - chrono::Duration::seconds(900)),
        resume_handle: Some(handle(Runtime::Claude)),
        safe_point: Some(SafePointRecord {
            reached_at: rfc3339(Utc::now()),
            parked_tool: Some("Bash".to_string()),
            parked_summary: Some("cargo test".to_string()),
        }),
        safe_point_miss: None,
        checkpoint_phase: Some("builder".to_string()),
        worktree: None,
        claim: Some(serde_json::json!({ "label": "loom:building", "on": "issue" })),
        lease_comment_id: None,
        lease_refreshed_at: None,
        log_path: None,
        overflow: false,
        role: None,
        timeout_remaining_secs: None,
        holds_issue_creation_mutex: false,
        stopped_at: Some(Utc::now()),
    }
}

fn role_run(dir: &Path, id: &str, role: &str) -> ManifestItem {
    ManifestItem {
        kind: ItemKind::RoleRun,
        issue: None,
        claim: None,
        checkpoint_phase: None,
        role: Some(role.to_string()),
        timeout_remaining_secs: Some(600),
        ..sweep(dir, id, 0)
    }
}

fn manifest(id: &str, phase: Phase, items: Vec<ManifestItem>) -> PauseManifest {
    PauseManifest {
        schema_version: pause_manifest::SCHEMA_VERSION,
        manifest_id: id.to_string(),
        phase,
        written_by: WrittenBy {
            version: "0.19.887".to_string(),
            ..WrittenBy::default()
        },
        roll: Roll {
            from_version: Some("0.19.887".to_string()),
            to_version: RUNNING.to_string(),
            to_artifact_sha256: Some("abcd".to_string()),
            target_source: None,
            staged_at: None,
            pause_started_at: Utc::now() - chrono::Duration::seconds(30),
            pause_completed_at: Some(Utc::now() - chrono::Duration::seconds(20)),
            pause_budget_secs: Some(120),
            min_resumable_age_secs: Some(300),
            max_age_secs: 900,
        },
        items,
        events: Vec::new(),
    }
}

fn write(plan: &ResumePlan, m: &PauseManifest) {
    pause_manifest::save(&plan.manifest_path, m).unwrap();
}

/// The archived manifest of a finished run.
fn archived(plan: &ResumePlan, id: &str) -> PauseManifest {
    let path = plan
        .manifest_path
        .parent()
        .unwrap()
        .join(format!("roll-pause-manifest.{id}.done.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no archive {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

fn item<'a>(m: &'a PauseManifest, id: &str) -> &'a ManifestItem {
    m.items
        .iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("no item {id}"))
}

fn finished(outcome: H5Outcome) -> PauseResumeStatus {
    match outcome {
        H5Outcome::Finished(status) => status,
        other => panic!("expected Finished, got {other:?}"),
    }
}

// ============================================================================
// The happy path, and its order
// ============================================================================

/// AC: paused sweep and role-run items, Claude and Codex, are resumed in
/// manifest order from their saved sessions; leases are refreshed first,
/// nothing is launched before health holds, dispatch is held throughout and
/// released at the end; the manifest is archived `resumed`.
#[test]
fn paused_items_are_resumed_in_order_after_lease_refresh_and_health_probation() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut codex = sweep(dir.path(), "s-codex", 2);
    codex.resume_handle = Some(ResumeHandle {
        session_store: Some("/profiles/agent-3".to_string()),
        sandbox: Some("workspace-write".to_string()),
        container: Some(serde_json::json!({ "name": "loom-codex-session-agent-3" })),
        ..handle(Runtime::Codex)
    });
    write(
        &plan,
        &manifest(
            "rp-happy",
            Phase::Paused,
            vec![
                sweep(dir.path(), "s-claude", 1),
                codex,
                role_run(dir.path(), "r-judge", "judge"),
            ],
        ),
    );
    let host = Arc::new(FakeHost {
        unhealthy: Mutex::new(2),
        ..FakeHost::default()
    });

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!((status.items, status.resumed), (3, 3));
    assert!(status.requeued_by_reason.is_empty());
    assert_eq!(status.resumed_on.as_deref(), Some("target"));
    assert_eq!((status.step.as_str(), status.phase.as_deref()), ("done", Some("resumed")));
    assert!(status.pause_to_resume_secs.is_some_and(|s| s < 900), "well below the lease TTL");
    assert!(!status.holding && !host.drain.is_draining(), "dispatch is released at the end");

    // Order: every lease refresh, then health, then the launches in manifest order.
    let calls = host.calls();
    let first = |prefix: &str| calls.iter().position(|c| c.starts_with(prefix)).unwrap();
    let last = |prefix: &str| calls.iter().rposition(|c| c.starts_with(prefix)).unwrap();
    assert!(last("lease ") < first("health"), "leases first: {calls:?}");
    assert!(
        last("health") < first("check "),
        "nothing is checked or launched before health holds"
    );
    assert_eq!(
        host.called("launch "),
        vec![
            format!("launch s-claude session={SID} count=1"),
            format!("launch s-codex session={SID} count=1"),
            format!("launch r-judge session={SID} count=1"),
        ]
    );
    assert_eq!(host.called("lease ").len(), 3);
    assert!(host.called("requeue ").is_empty());
    assert!(calls.contains(&"settle s-claude as s-claude-r1".to_string()));
    assert_eq!(calls.last().map(String::as_str), Some("finish rp-happy"));
    // Dispatch was held while H5 worked.
    assert!(host
        .statuses
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.step == "probation" && s.holding));

    // The manifest is archived, terminal, and gone from the live path.
    assert!(!plan.manifest_path.exists());
    let done = archived(&plan, "rp-happy");
    assert_eq!(done.phase, Phase::Resumed);
    for id in ["s-claude", "s-codex", "r-judge"] {
        let it = item(&done, id);
        assert_eq!(it.status, ItemStatus::Resumed, "{id}");
        assert_eq!(it.resume_handle.as_ref().unwrap().resume_count, 1);
        assert!(it.lease_refreshed_at.is_some());
    }
    assert!(done.events.iter().any(|e| e.event == "probation_passed"));

    // Telemetry: one item event each, and the summary with both versions.
    let items = host.topic("daemon.roll.item");
    assert_eq!(items.len(), 3);
    assert!(items
        .iter()
        .all(|p| p["stage"] == "resume" && p["status"] == "resumed"));
    assert_eq!(items[0]["new_item_id"], "s-claude-r1");
    let summary = &host.topic("daemon.roll.paused.resumed")[0];
    assert_eq!(summary["resumed_on"], "target");
    assert_eq!(summary["from_version"], "0.19.887");
    assert_eq!(summary["to_version"], RUNNING);
    assert_eq!(summary["running_version"], RUNNING);
    assert_eq!(summary["resumed"], 3);
    assert!(summary["pause_to_resume_secs"].is_u64());
}

/// The launch H5 builds carries the saved session, the lineage, the carried
/// first start, and a prompt naming the roll and the parked call.
#[test]
fn the_launch_carries_the_session_the_lineage_and_the_parked_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut it = sweep(dir.path(), "s-1", 7);
    it.resume_handle = Some(ResumeHandle {
        session_store: Some("/profiles/agent-3".to_string()),
        account: Some("agent-3".to_string()),
        container: Some(
            serde_json::json!({ "name": "loom-codex-session-agent-3", "account": "agent-3" }),
        ),
        sandbox: Some("danger-full-access".to_string()),
        resume_count: 1,
        lease_sweep_id: Some("s-0".to_string()),
        ..handle(Runtime::Codex)
    });
    let m = manifest("rp-1", Phase::Paused, vec![it.clone()]);
    let launch = launch_of(&it, &m).unwrap();
    assert_eq!((launch.runtime.as_str(), launch.session_id.as_str()), ("codex", SID));
    assert_eq!((launch.resume_of.as_str(), launch.resume_count), ("s-1", 2));
    assert_eq!(launch.agent_started_at, it.agent_started_at.map(|t| t.to_rfc3339()));
    assert_eq!(launch.session_store.as_deref(), Some("/profiles/agent-3"));
    assert_eq!(launch.container.as_deref(), Some("loom-codex-session-agent-3"));
    assert_eq!(launch.sandbox.as_deref(), Some("danger-full-access"));
    assert_eq!(launch.lease_sweep_id.as_deref(), Some("s-0"));
    assert!(launch.prompt.contains("0.19.887 -> 0.19.900"), "{}", launch.prompt);
    assert!(launch.prompt.contains("Bash: cargo test") && launch.prompt.contains("did NOT run"));

    it.resume_handle.as_mut().unwrap().session_id = None;
    assert!(launch_of(&it, &m).is_none());
}

// ============================================================================
// Every H5 requeue reason (design §9)
// ============================================================================

/// AC: each reason a cross-check, a launch or a start can produce requeues the
/// item with exactly that reason, records it, and counts it.
#[test]
fn every_refusal_requeues_with_its_reason() {
    for (reason, at) in [
        ("lease-lost", "check"),
        ("issue-parked", "check"),
        ("worktree-changed", "check"),
        ("session-store-unavailable", "check"),
        ("session-down", "check"),
        ("role-disabled", "check"),
        ("guard-refused:claim-lock", "check"),
        ("guard-refused:runtime-admission", "launch"),
        ("session-resume-failed", "launch"),
        ("session-resume-failed", "start"),
        ("session-down", "start"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path());
        write(
            &plan,
            &manifest(
                "rp-reason",
                Phase::Paused,
                vec![sweep(dir.path(), "bad", 1), sweep(dir.path(), "good", 2)],
            ),
        );
        let host = Arc::new(FakeHost::default());
        match at {
            "check" => {
                host.refuse_check
                    .lock()
                    .unwrap()
                    .insert("bad".to_string(), refusal(reason));
            }
            "launch" => {
                host.refuse_launch
                    .lock()
                    .unwrap()
                    .insert("bad".to_string(), refusal(reason));
            }
            _ => {
                host.die
                    .lock()
                    .unwrap()
                    .insert("bad".to_string(), reason.to_string());
            }
        }

        let status = finished(run_h5(host.clone(), &plan));

        let case = format!("{reason} at {at}");
        assert_eq!(
            (status.resumed, status.requeued_by_reason.get(reason)),
            (1, Some(&1)),
            "{case}"
        );
        assert!(
            host.calls()
                .contains(&format!("requeue bad {reason} forge=true")),
            "{case}: {:?}",
            host.calls()
        );
        assert_eq!(host.called("abandon ").len(), usize::from(at == "start"), "{case}");
        let done = archived(&plan, "rp-reason");
        let bad = item(&done, "bad");
        assert_eq!(
            (bad.disposition.clone(), bad.status.clone()),
            (Disposition::Requeue, ItemStatus::Requeued),
            "{case}"
        );
        assert_eq!(bad.reason.as_deref(), Some(reason), "{case}");
        assert_eq!(
            item(&done, "good").status,
            ItemStatus::Resumed,
            "{case}: one refusal does not stop the rest"
        );
        assert!(
            host.topic("daemon.roll.item")
                .iter()
                .any(|p| p["item_id"] == "bad" && p["reason"] == reason),
            "{case}"
        );
    }
}

/// `issue-closed` is recorded, never requeued on the forge (#9463).
#[test]
fn a_closed_issue_is_recorded_and_released_without_a_forge_write() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-closed", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost::default());
    host.refuse_check
        .lock()
        .unwrap()
        .insert("s-1".to_string(), refusal("issue-closed"));
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!(status.requeued_by_reason["issue-closed"], 1);
    assert_eq!(host.called("requeue "), vec!["requeue s-1 issue-closed forge=false"]);
}

/// `resume-attempts-exhausted`, `session-not-resumable` and an unknown enum
/// value are decided from the manifest alone: nothing is checked or launched.
#[test]
fn items_the_manifest_already_rules_out_are_requeued_without_a_launch() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut spent = sweep(dir.path(), "spent", 1);
    spent.resume_handle.as_mut().unwrap().resume_count = MAX_ROLL_RESUMES;
    let mut no_session = sweep(dir.path(), "nosess", 2);
    no_session.resume_handle.as_mut().unwrap().session_id = None;
    let mut alien = sweep(dir.path(), "alien", 3);
    alien.kind = ItemKind::Unknown("epic".to_string());
    let mut two = sweep(dir.path(), "two", 4);
    two.resume_handle.as_mut().unwrap().resume_count = MAX_ROLL_RESUMES - 1;
    write(&plan, &manifest("rp-ruled", Phase::Paused, vec![spent, no_session, alien, two]));
    let host = Arc::new(FakeHost::default());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.requeued_by_reason["resume-attempts-exhausted"], 1);
    assert_eq!(status.requeued_by_reason["session-not-resumable"], 1);
    assert_eq!(status.requeued_by_reason["unknown-kind-epic"], 1);
    assert_eq!(status.resumed, 1, "a session resumed twice before may be resumed a third time");
    assert_eq!(host.called("launch ").len(), 1);
    assert_eq!(host.called("lease "), vec!["lease two"], "only what will be resumed");
    assert_eq!(
        item(&archived(&plan, "rp-ruled"), "two")
            .resume_handle
            .as_ref()
            .unwrap()
            .resume_count,
        3
    );
}

/// `resume-timeout`: items not reached before `resumeBudgetSecs` runs out.
#[test]
fn items_not_reached_within_the_resume_budget_are_requeued_as_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan = plan(dir.path());
    plan.tuning.resume_budget = Duration::from_millis(150);
    write(
        &plan,
        &manifest(
            "rp-timeout",
            Phase::Paused,
            vec![
                sweep(dir.path(), "a", 1),
                sweep(dir.path(), "b", 2),
                sweep(dir.path(), "c", 3),
            ],
        ),
    );
    let host = Arc::new(FakeHost {
        launch_delay: Duration::from_millis(200),
        ..FakeHost::default()
    });
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!(status.resumed, 1, "the one launched inside the budget");
    assert_eq!(status.requeued_by_reason[REASON_TIMEOUT], 2);
    assert_eq!(host.called("launch ").len(), 1);
    let done = archived(&plan, "rp-timeout");
    assert_eq!(item(&done, "c").reason.as_deref(), Some(REASON_TIMEOUT));
}

// ============================================================================
// Stale, unreadable and already-finished manifests
// ============================================================================

/// AC: a stale manifest requeues instead of resuming, and is archived
/// `abandoned`. Health probation is not waited for: nothing will be launched.
#[test]
fn a_stale_manifest_requeues_everything_and_is_archived_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = manifest(
        "rp-stale",
        Phase::Paused,
        vec![
            sweep(dir.path(), "s-1", 1),
            role_run(dir.path(), "r-1", "judge"),
        ],
    );
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(901);
    write(&plan, &m);
    let host = Arc::new(FakeHost::default());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.load, "stale");
    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (0, 2));
    assert!(host.called("launch ").is_empty() && host.called("lease ").is_empty());
    assert!(host.called("health").is_empty());
    assert_eq!(archived(&plan, "rp-stale").phase, Phase::Abandoned);
    assert_eq!(host.topic("daemon.roll.paused.resumed")[0]["phase"], "abandoned");
}

/// A corrupt manifest, or one from a newer schema, resumes nothing, is
/// reported once, and is left exactly where it is. It never panics.
#[test]
fn an_unreadable_manifest_is_reported_and_left_to_restart_recovery() {
    for (body, load) in [
        ("{ not json".to_string(), "corrupt"),
        (
            r#"{"schema_version": 2, "items": "a new shape"}"#.to_string(),
            "unknown-version",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(dir.path());
        std::fs::create_dir_all(plan.manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&plan.manifest_path, &body).unwrap();
        let host = Arc::new(FakeHost::default());
        let H5Outcome::Unreadable(status) = run_h5(host.clone(), &plan) else {
            panic!("{load}: expected Unreadable");
        };
        assert_eq!((status.load.as_str(), status.step.as_str()), (load, "unreadable"));
        assert_eq!(
            std::fs::read_to_string(&plan.manifest_path).unwrap(),
            body,
            "{load}: untouched"
        );
        assert_eq!(host.topic("daemon.roll.manifest.unreadable")[0]["outcome"], load);
        assert!(host.calls().is_empty(), "{load}: nothing is held, checked or launched");
        assert!(!host.drain.is_draining());
    }
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run_h5(Arc::new(FakeHost::default()), &plan(dir.path())), H5Outcome::NoManifest);
}

/// A manifest a dying process had already finished is archived, not redone.
#[test]
fn a_manifest_that_is_already_terminal_is_only_archived() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut it = sweep(dir.path(), "s-1", 1);
    it.status = ItemStatus::Resumed;
    write(&plan, &manifest("rp-term", Phase::Resumed, vec![it]));
    let host = Arc::new(FakeHost::default());
    finished(run_h5(host.clone(), &plan));
    assert_eq!(host.calls(), vec!["finish rp-term"]);
    assert_eq!(archived(&plan, "rp-term").phase, Phase::Resumed);
}

// ============================================================================
// Crashes in H4 and H5
// ============================================================================

/// AC: a crash during H4 (`phase = pausing`) is finished by the next start.
/// An item whose agent had parked is resumed; one with no safe-point record
/// is requeued (`pause-budget-missed`); an H4 requeue left `planned` gets its
/// forge writes.
#[test]
fn an_interrupted_h4_is_finished_by_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let stopping = |id: &str, issue: u32, status: ItemStatus| {
        let mut it = sweep(dir.path(), id, issue);
        it.status = status;
        it.safe_point = None;
        it
    };
    let mut young = sweep(dir.path(), "young", 4);
    young.disposition = Disposition::Requeue;
    young.status = ItemStatus::Planned;
    young.reason = Some("young-agent-reset".to_string());
    let mut m = manifest(
        "rp-h4",
        Phase::Pausing,
        vec![
            stopping("parked", 1, ItemStatus::Stopping),
            stopping("running", 2, ItemStatus::Stopping),
            stopping("unsignalled", 3, ItemStatus::Planned),
            young,
        ],
    );
    m.roll.pause_completed_at = None;
    write(&plan, &m);
    let host = Arc::new(FakeHost::default());
    host.parked.lock().unwrap().insert("parked".to_string());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.resumed, 1);
    assert_eq!(status.requeued_by_reason[REASON_BUDGET_MISSED], 2);
    assert_eq!(status.requeued_by_reason["young-agent-reset"], 1);
    assert_eq!(host.called("launch "), vec![format!("launch parked session={SID} count=1")]);
    let done = archived(&plan, "rp-h4");
    assert_eq!(done.phase, Phase::Resumed);
    let parked = item(&done, "parked");
    assert_eq!(parked.status, ItemStatus::Resumed);
    assert_eq!(parked.safe_point.as_ref().unwrap().parked_tool.as_deref(), Some("Edit"));
    assert_eq!(item(&done, "running").reason.as_deref(), Some(REASON_BUDGET_MISSED));
    // Every agent H4 left behind is checked for residue before anything else.
    assert_eq!(host.called("residue ").len(), 4);
}

/// AC: a crash during H5 (`phase = resuming`) does not double-launch. An item
/// marked `resumed` is left alone; one whose relaunch had started is found
/// running and marked; only the rest is launched.
#[test]
fn a_crash_during_h5_never_launches_an_item_twice() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut done_before = sweep(dir.path(), "done", 1);
    done_before.status = ItemStatus::Resumed;
    write(
        &plan,
        &manifest(
            "rp-h5",
            Phase::Resuming,
            vec![
                done_before,
                sweep(dir.path(), "started", 2),
                sweep(dir.path(), "pending", 3),
            ],
        ),
    );
    let host = Arc::new(FakeHost::default());
    host.running
        .lock()
        .unwrap()
        .insert("started".to_string(), "started-r1".to_string());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(host.called("launch "), vec![format!("launch pending session={SID} count=1")]);
    assert_eq!(status.resumed, 2, "found running + launched now");
    assert!(
        host.called("residue ")
            .iter()
            .all(|c| !c.contains("done") && !c.contains("started")),
        "a running resume is never reaped as residue: {:?}",
        host.calls()
    );
    let done = archived(&plan, "rp-h5");
    for id in ["done", "started", "pending"] {
        assert_eq!(item(&done, id).status, ItemStatus::Resumed, "{id}");
    }
    assert!(done.events.iter().any(|e| e.event == "resumed"
        && e.item.as_deref() == Some("started")
        && e.detail
            .as_deref()
            .is_some_and(|d| d.contains("found already running"))));
}

// The drain-replaces-the-hold cases are in `tests/exits.rs`.

// ============================================================================
// Health and holds
// ============================================================================

/// Nothing is resumed on a binary that does not stay healthy: a failure after
/// health had held restarts the window, is reported, and the resume waits.
#[test]
fn a_health_failure_restarts_probation_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-health", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));

    /// Healthy once, then one failure, then healthy.
    struct Flaky(FakeHost, Mutex<u32>);
    impl ResumeHost for Flaky {
        fn health_sample(&self) -> Result<(), String> {
            let mut n = self.1.lock().unwrap();
            *n += 1;
            if *n == 2 {
                return Err("the heartbeat is 400s old".to_string());
            }
            Ok(())
        }
        fn hold_dispatch(&self) -> ResumeHold {
            self.0.hold_dispatch()
        }
        fn safe_point(&self, i: &ManifestItem, r: &PauseRequest) -> Option<SafePointRecord> {
            self.0.safe_point(i, r)
        }
        fn reap_residue(&self, i: &ManifestItem) -> TeardownReport {
            self.0.reap_residue(i)
        }
        fn refresh_lease(&self, i: &ManifestItem, t: Duration) -> Result<(), String> {
            self.0.refresh_lease(i, t)
        }
        fn already_resumed(&self, i: &ManifestItem) -> Option<String> {
            self.0.already_resumed(i)
        }
        fn check(&self, i: &ManifestItem, l: &RollResumeLaunch) -> Result<(), RollResumeRefusal> {
            self.0.check(i, l)
        }
        fn launch(
            &self,
            i: &ManifestItem,
            l: &RollResumeLaunch,
            w: Duration,
        ) -> Result<Launched, RollResumeRefusal> {
            self.0.launch(i, l, w)
        }
        fn liveness(&self, i: &ManifestItem, l: &Launched) -> Liveness {
            self.0.liveness(i, l)
        }
        fn abandon(&self, i: &ManifestItem, l: &Launched) {
            self.0.abandon(i, l)
        }
        fn settle(&self, m: &str, i: &ManifestItem, n: &str) {
            self.0.settle(m, i, n)
        }
        fn requeue(&self, i: &ManifestItem, n: &RollRequeueNotice, f: bool) -> Result<(), String> {
            self.0.requeue(i, n, f)
        }
        fn recover(&self, m: &str, i: &ManifestItem) {
            self.0.recover(m, i)
        }
        fn finish(&self, m: &str, n: &str) {
            self.0.finish(m, n)
        }
        fn emit(&self, t: &str, p: serde_json::Value) {
            self.0.emit(t, p)
        }
        fn publish(&self, s: &PauseResumeStatus) {
            self.0.publish(s)
        }
    }
    let host = Arc::new(Flaky(FakeHost::default(), Mutex::new(0)));

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!((status.health_failures, status.resumed), (1, 1));
    let failed = &host.0.topic("daemon.roll.health_failed")[0];
    assert!(failed["reason"].as_str().unwrap().contains("heartbeat"), "{failed}");
    assert!(archived(&plan, "rp-health")
        .events
        .iter()
        .any(|e| e.event == "health_failed"));
}

/// A binary that never becomes healthy resumes nothing. Once the manifest is
/// stale its claims are given back instead of being held forever.
#[test]
fn a_binary_that_never_gets_healthy_requeues_once_the_manifest_goes_stale() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = manifest("rp-sick", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]);
    // Fresh now, stale in about a second.
    m.roll.max_age_secs = 2;
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(1);
    write(&plan, &m);
    let host = Arc::new(FakeHost {
        unhealthy: Mutex::new(u32::MAX),
        ..FakeHost::default()
    });
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (0, 1));
    assert!(host.called("launch ").is_empty(), "never launched on an unhealthy binary");
    assert_eq!(archived(&plan, "rp-sick").phase, Phase::Abandoned);
}

/// A hold already in force (an operator stop, a fleet pause) is respected: H5
/// waits for it, and resumes once it lifts.
#[test]
fn h5_waits_for_a_hold_already_in_force() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-wait", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost {
        hold_blocked: Mutex::new(5),
        ..FakeHost::default()
    });
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!(status.resumed, 1);
    assert_eq!(*host.hold_blocked.lock().unwrap(), 0, "it waited the hold out");
}

/// #10979: a host the fleet store says is `paused` finishes its paused
/// agents (in-flight work, which a fleet pause lets finish) and then must NOT
/// dispatch: the fleet pause is still in force when H5 ends, whether it was in
/// force at startup or arrived while H5 was working.
#[test]
fn a_fleet_paused_host_does_not_dispatch_after_h5_completes() {
    // In force at startup (the startup fleet-sync pass re-applied `paused`
    // before H5 was spawned).
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-fleet", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost::default());
    assert!(host
        .drain
        .hold_for_fleet_state("fleet/state.yml says paused".to_string()));

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.resumed, 1, "its paused agent is finished, not abandoned");
    assert!(host.drain.is_draining(), "dispatch is still paused after H5");
    assert!(host.drain.is_fleet_held(), "by the fleet pause, which H5 never released");
    assert!(!host.drain.is_roll_resume_held());
    assert_eq!(archived(&plan, "rp-fleet").phase, Phase::Resumed);

    // Arriving while H5 holds dispatch (a sync pass during the resume).
    let dir = tempfile::tempdir().unwrap();
    let plan = ResumePlan {
        manifest_path: dir.path().join("state").join(pause_manifest::MANIFEST_FILE),
        ..plan
    };
    write(&plan, &manifest("rp-fleet-2", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));

    struct PausedMidway(FakeHost);
    impl ResumeHost for PausedMidway {
        fn check(&self, i: &ManifestItem, l: &RollResumeLaunch) -> Result<(), RollResumeRefusal> {
            // The store flips to `paused` just before the relaunch.
            assert!(self
                .0
                .drain
                .hold_for_fleet_state("fleet says paused".to_string()));
            self.0.check(i, l)
        }
        fn health_sample(&self) -> Result<(), String> {
            self.0.health_sample()
        }
        fn hold_dispatch(&self) -> ResumeHold {
            self.0.hold_dispatch()
        }
        fn safe_point(&self, i: &ManifestItem, r: &PauseRequest) -> Option<SafePointRecord> {
            self.0.safe_point(i, r)
        }
        fn reap_residue(&self, i: &ManifestItem) -> TeardownReport {
            self.0.reap_residue(i)
        }
        fn refresh_lease(&self, i: &ManifestItem, t: Duration) -> Result<(), String> {
            self.0.refresh_lease(i, t)
        }
        fn already_resumed(&self, i: &ManifestItem) -> Option<String> {
            self.0.already_resumed(i)
        }
        fn launch(
            &self,
            i: &ManifestItem,
            l: &RollResumeLaunch,
            w: Duration,
        ) -> Result<Launched, RollResumeRefusal> {
            self.0.launch(i, l, w)
        }
        fn liveness(&self, i: &ManifestItem, l: &Launched) -> Liveness {
            self.0.liveness(i, l)
        }
        fn abandon(&self, i: &ManifestItem, l: &Launched) {
            self.0.abandon(i, l);
        }
        fn settle(&self, m: &str, i: &ManifestItem, n: &str) {
            self.0.settle(m, i, n);
        }
        fn requeue(&self, i: &ManifestItem, n: &RollRequeueNotice, f: bool) -> Result<(), String> {
            self.0.requeue(i, n, f)
        }
        fn recover(&self, m: &str, i: &ManifestItem) {
            self.0.recover(m, i);
        }
        fn finish(&self, m: &str, n: &str) {
            self.0.finish(m, n);
        }
        fn emit(&self, t: &str, p: serde_json::Value) {
            self.0.emit(t, p);
        }
        fn publish(&self, s: &PauseResumeStatus) {
            self.0.publish(s);
        }
    }
    let host = Arc::new(PausedMidway(FakeHost::default()));
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!(status.resumed, 1);
    assert!(
        host.0.drain.is_draining() && host.0.drain.is_fleet_held(),
        "still paused after H5"
    );
}

/// An operator stop is not a fleet pause: H5 resumes nothing under it, and
/// gives the claims back once the manifest goes stale.
#[test]
fn an_operator_stop_holds_the_resume_until_the_manifest_goes_stale() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut m = manifest("rp-stopped", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]);
    m.roll.max_age_secs = 2;
    m.roll.pause_started_at = Utc::now() - chrono::Duration::seconds(1);
    write(&plan, &m);
    let host = Arc::new(FakeHost::default());
    // A then-exit operator drain: dispatch is paused, and not by the fleet.
    let _ = host.drain.begin(Duration::from_secs(600), false, true);

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!((status.resumed, status.requeued_by_reason[REASON_STALE]), (0, 1));
    assert!(host.called("launch ").is_empty());
    assert!(host.drain.is_draining(), "the operator's drain is untouched");
}

/// A saved session is never resumed beside a survivor of its old tree.
#[test]
fn an_item_whose_old_tree_survives_the_reap_is_requeued_not_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-surv", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost::default());
    host.residue.lock().unwrap().insert("s-1".to_string());
    host.survivors.lock().unwrap().push(4243);
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!(status.requeued_by_reason["guard-refused:residue-alive"], 1);
    assert!(host.called("launch ").is_empty());
}

// ============================================================================
// Items that are not resumed
// ============================================================================

/// An agent that ended by itself during the pause is completed work: it is
/// released, with no forge write and no crash recovery. An H4 requeue is
/// released too (H4 never removes a lock). A PR-set sweep is never resumed.
/// A requeue whose forge write fails is handed to restart recovery.
#[test]
fn exited_requeued_pr_set_and_failed_requeue_items_each_end_their_own_way() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    let mut exited = sweep(dir.path(), "exited", 1);
    exited.status = ItemStatus::Exited;
    exited.reason = Some("exited-before-safe-point".to_string());
    let mut requeued = sweep(dir.path(), "h4-requeued", 2);
    requeued.disposition = Disposition::Requeue;
    requeued.status = ItemStatus::Requeued;
    requeued.reason = Some("young-agent-reset".to_string());
    let mut prs = sweep(dir.path(), "sweep-prs-5-6-1000", 0);
    prs.issue = None;
    let mut flaky = sweep(dir.path(), "flaky", 4);
    flaky.disposition = Disposition::Requeue;
    flaky.status = ItemStatus::Planned;
    flaky.reason = Some("pause-budget-missed".to_string());
    write(&plan, &manifest("rp-ends", Phase::Paused, vec![exited, requeued, prs, flaky]));
    let host = Arc::new(FakeHost::default());
    host.requeue_fails
        .lock()
        .unwrap()
        .insert("flaky".to_string());

    let status = finished(run_h5(host.clone(), &plan));

    let calls = host.calls();
    assert!(
        calls.contains(&"requeue exited exited-before-safe-point forge=false".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"requeue h4-requeued young-agent-reset forge=false".to_string()));
    assert!(calls.contains(&format!("requeue sweep-prs-5-6-1000 {REASON_PR_SET} forge=true")));
    assert!(calls.contains(&"recover flaky".to_string()), "{calls:?}");
    assert!(
        !calls.contains(&"recover exited".to_string()),
        "completed work is not crash-recovered"
    );
    assert!(host.called("launch ").is_empty());
    // Every item H4 left behind is checked for residue, exited ones included
    // (H4 runs no teardown for an agent that ended by itself).
    assert!(calls.contains(&"residue h4-requeued".to_string()));
    assert!(calls.contains(&"residue exited".to_string()));
    assert_eq!((status.completed, status.recovered, status.resumed), (1, 1, 0));
    assert_eq!(status.requeued_by_reason["young-agent-reset"], 1);
    assert_eq!(status.requeued_by_reason[REASON_PR_SET], 1);
    let done = archived(&plan, "rp-ends");
    assert_eq!(item(&done, "exited").status, ItemStatus::Completed);
    assert_eq!(item(&done, "flaky").status, ItemStatus::Failed);
    assert_eq!(item(&done, "flaky").reason.as_deref(), Some("pause-budget-missed"));
    // No item leaves the manifest without a terminal status.
    assert!(done.items.iter().all(|i| !matches!(
        i.status,
        ItemStatus::Planned | ItemStatus::Stopping | ItemStatus::Paused
    )));
}

/// AC: nothing from a manifest item's tree is alive when it is resumed; what
/// is found is reaped and recorded before the launch.
#[test]
fn residue_is_reaped_and_recorded_before_the_item_is_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    write(&plan, &manifest("rp-res", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost::default());
    host.residue.lock().unwrap().insert("s-1".to_string());
    let status = finished(run_h5(host.clone(), &plan));
    assert_eq!((status.residue_reaped, status.resumed), (1, 1));
    let calls = host.calls();
    let at = |p: &str| calls.iter().position(|c| c.starts_with(p)).unwrap();
    assert!(at("residue s-1") < at("launch s-1"));
    assert_eq!(
        host.topic("daemon.roll.item.residue_reaped")[0]["pids"],
        serde_json::json!([4242, 4243])
    );
    assert!(archived(&plan, "rp-res")
        .events
        .iter()
        .any(|e| e.event == "residue_reaped"));
}

/// Finished manifests are kept for audit, the newest ten only.
#[test]
fn only_the_newest_ten_archives_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(dir.path());
    for n in 0..12 {
        write(&plan, &manifest(&format!("rp-{n:02}"), Phase::Paused, Vec::new()));
        finished(run_h5(Arc::new(FakeHost::default()), &plan));
        // Distinct mtimes, oldest first.
        std::thread::sleep(Duration::from_millis(15));
    }
    let mut kept: Vec<String> = std::fs::read_dir(plan.manifest_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".done.json"))
        .collect();
    kept.sort();
    assert_eq!(kept.len(), ARCHIVE_KEEP);
    assert_eq!(kept.first().unwrap(), "roll-pause-manifest.rp-02.done.json");
}

// ============================================================================
// Rollback
// ============================================================================

/// AC: a rolled-back binary at or after #10715 resumes the manifest and
/// records `resumed_on = rollback`. Rollback safety: the target that did not
/// take is held back, so the old binary does not pause the host for it again
/// at once; a later roll that takes clears the record.
#[test]
fn a_rolled_back_binary_resumes_and_holds_the_failed_target_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan = plan(dir.path());
    // This process is the OLD binary: the roll to 0.19.900 did not take.
    plan.running_version = "0.19.887".to_string();
    write(&plan, &manifest("rp-back", Phase::Paused, vec![sweep(dir.path(), "s-1", 1)]));
    let host = Arc::new(FakeHost::default());

    let status = finished(run_h5(host.clone(), &plan));

    assert_eq!(status.resumed_on.as_deref(), Some("rollback"));
    assert_eq!(status.resumed, 1, "the paused agent is resumed all the same");
    let summary = &host.topic("daemon.roll.paused.resumed")[0];
    assert_eq!(summary["resumed_on"], "rollback");
    assert_eq!(
        (summary["running_version"].as_str(), summary["to_version"].as_str()),
        (Some("0.19.887"), Some(RUNNING))
    );
    let done = archived(&plan, "rp-back");
    assert!(done.events.iter().any(|e| e.event == "resumed_on"
        && e.detail
            .as_deref()
            .is_some_and(|d| d.starts_with("rollback"))));

    // The failed target is on record, and gates the next roll to it.
    let state = plan.manifest_path.parent().unwrap();
    let failed = attempt::load(state).expect("the failed target is recorded");
    assert_eq!((failed.version.as_str(), failed.attempts), (RUNNING, 1));
    assert!(attempt::gate(state, Some(RUNNING), Some("abcd"), Utc::now()).is_some());
    assert_eq!(attempt::gate(state, Some("0.19.901"), None, Utc::now()), None);

    // The roll is tried again later and takes: the record is cleared.
    plan.running_version = RUNNING.to_string();
    write(&plan, &manifest("rp-took", Phase::Paused, Vec::new()));
    finished(run_h5(Arc::new(FakeHost::default()), &plan));
    assert_eq!(attempt::load(state), None);
}

#[path = "tests/exits.rs"]
mod exits;
#[path = "tests/fake_host.rs"]
mod fake_host;
#[path = "tests/integration.rs"]
mod integration;
use fake_host::{refusal, FakeHost};
