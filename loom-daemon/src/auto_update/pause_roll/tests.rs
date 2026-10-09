//! H4 pause tests (#10831). No live agents: the integration test drives real
//! process trees and a real sweep registry with a fake `gh`; the rest use a
//! scripted host.

use super::host::{sweep_candidates, Candidate, PauseHost};
use super::teardown::{self, TeardownReport};
use super::*;
use crate::auto_update::pause_manifest::{ItemKind, LoadOutcome, ResumeHandle, Runtime};
use crate::event_bus::EventBus;
use crate::ipc::{AbortOutcome, DrainBegin, DrainOrigin, PauseRollStatus};
use crate::sweep_registry::test_support;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use crate::workspace_pool::WorkspacePool;
use serial_test::serial;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub(super) const SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";

pub(super) fn tuning(budget_ms: u64) -> PauseRollTuning {
    PauseRollTuning {
        pause_budget: Duration::from_millis(budget_ms),
        poll: Duration::from_millis(20),
        pending_settle: Duration::from_secs(5),
        forge_floor: Duration::from_millis(400),
        ..PauseRollTuning::defaults(Duration::from_secs(900))
    }
}

pub(super) fn target() -> RollTarget {
    RollTarget {
        source: TargetSource::Floor,
        to_version: Some("0.19.900".to_string()),
        to_artifact_sha256: Some("abcd".to_string()),
        label: Some("v0.19.900@abcd".to_string()),
    }
}

/// Begin a pause roll on `drain` and return the plan for it.
pub(super) fn begin(drain: &DrainState, dir: &Path, tuning: PauseRollTuning) -> PausePlan {
    let progress = PauseRollStatus {
        budget_secs: tuning.pause_budget.as_secs(),
        ..PauseRollStatus::default()
    };
    let DrainBegin::Started { generation, .. } =
        drain.begin_pause_roll(tuning.pause_budget, progress)
    else {
        panic!("the pause roll must start");
    };
    PausePlan {
        target: target(),
        tuning,
        manifest_path: dir.join("state").join(pause_manifest::MANIFEST_FILE),
        from_version: "0.19.887".to_string(),
        generation,
        staged_at: Utc::now(),
        supervisor: Some("launchd".to_string()),
    }
}

pub(super) fn load(plan: &PausePlan) -> PauseManifest {
    match pause_manifest::load(&plan.manifest_path, Utc::now()) {
        LoadOutcome::Loaded(m) => m,
        other => panic!("expected a loadable manifest, got {other:?}"),
    }
}

pub(super) fn item<'a>(m: &'a PauseManifest, id: &str) -> &'a ManifestItem {
    m.items
        .iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("no item {id} in {:?}", m.items))
}

/// Stand in for the agent's pause hook: once a pause is requested for
/// `pause_dir`'s item, run the real hook for one tool call, which parks and
/// writes the safe-point record.
pub(super) fn spawn_parking_agent(pause_dir: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !roll_pause::is_requested(&pause_dir) {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let env = roll_pause::HookEnv {
            item: pause_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            pause_root: pause_dir.parent().unwrap().to_path_buf(),
            ledger: true,
            park: Duration::from_secs(20),
            poll: Duration::from_millis(10),
            runtime: "claude".to_string(),
            harness_pid: None,
        };
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "toolu_1",
            "tool_input": {"command": "cargo test"}, "session_id": SID,
        });
        // Parks until the request is withdrawn (step 9) or the park runs out.
        let _ = roll_pause::run_hook(&env, &payload.to_string());
    })
}

// ============================================================================
// Scripted host
// ============================================================================

pub(super) type Hook = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub(super) struct FakeHost {
    pub(super) cands: Vec<Candidate>,
    pub(super) pending: AtomicUsize,
    pub(super) dead: Mutex<BTreeSet<String>>,
    pub(super) torn: Mutex<Vec<String>>,
    /// Items whose process group the stop bound killed (#11051).
    pub(super) forced: Mutex<Vec<String>>,
    /// Held item -> the pause run holding it.
    pub(super) held: Mutex<BTreeMap<String, String>>,
    /// The pause runs holding dispatch closed.
    pub(super) closed: Mutex<BTreeSet<String>>,
    pub(super) leases: Mutex<Vec<String>>,
    pub(super) requeued: Mutex<Vec<(String, String)>>,
    pub(super) events: Mutex<Vec<(String, serde_json::Value)>>,
    pub(super) requeue_delay: Duration,
    pub(super) requeue_fails: bool,
    /// Called with the item id at the start of each teardown.
    pub(super) on_teardown: Option<Hook>,
}

impl PauseHost for FakeHost {
    fn close_dispatch(&self, closed: bool, run: &str) {
        let mut set = self.closed.lock().unwrap();
        if closed {
            set.insert(run.to_string());
        } else {
            set.remove(run);
        }
    }
    fn pending_dispatches(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
    fn snapshot(&self) -> Vec<Candidate> {
        self.cands.clone()
    }
    fn hold(&self, c: &Candidate, held: bool, run: &str) {
        let mut map = self.held.lock().unwrap();
        if held {
            map.insert(c.id.clone(), run.to_string());
        } else if map.get(&c.id).is_some_and(|by| by == run) {
            map.remove(&c.id);
        }
    }
    fn is_alive(&self, c: &Candidate) -> bool {
        !self.dead.lock().unwrap().contains(&c.id)
    }
    fn teardown(&self, c: &Candidate) -> TeardownReport {
        if let Some(hook) = &self.on_teardown {
            hook(&c.id);
        }
        self.torn.lock().unwrap().push(c.id.clone());
        self.dead.lock().unwrap().insert(c.id.clone());
        TeardownReport::default()
    }
    fn force_kill(&self, c: &Candidate) -> bool {
        self.forced.lock().unwrap().push(c.id.clone());
        self.dead.lock().unwrap().insert(c.id.clone());
        true
    }
    fn refresh_lease(&self, c: &Candidate, _timeout: Duration) -> Result<(), String> {
        self.leases.lock().unwrap().push(c.id.clone());
        Ok(())
    }
    fn requeue(&self, c: &Candidate, notice: &RollRequeueNotice) -> Result<(), String> {
        std::thread::sleep(self.requeue_delay);
        if self.requeue_fails {
            return Err("forge down".to_string());
        }
        self.requeued
            .lock()
            .unwrap()
            .push((c.id.clone(), notice.reason.clone()));
        Ok(())
    }
    fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((topic.to_string(), payload));
    }
}

impl FakeHost {
    pub(super) fn item_events(&self) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "daemon.roll.item")
            .map(|(_, p)| p.clone())
            .collect()
    }
}

/// A sweep candidate `age_secs` old with a resumable Claude session.
pub(super) fn cand(dir: &Path, id: &str, issue: u32, age_secs: i64) -> Candidate {
    Candidate {
        id: id.to_string(),
        kind: ItemKind::Sweep,
        repo: dir.to_path_buf(),
        pause_dir: Some(roll_pause::item_dir(&dir.join("pause"), id)),
        issue: Some(issue),
        pid: Some(1_000_000 + issue),
        pgid: Some(1_000_000 + issue),
        scope_unit: None,
        agent_started_at: Some(Utc::now() - chrono::Duration::seconds(age_secs)),
        run_started_at: Some(Utc::now() - chrono::Duration::seconds(age_secs)),
        proc_started_at: None,
        resume_handle: Some(ResumeHandle {
            runtime: Runtime::Claude,
            session_id: Some(SID.to_string()),
            session_store: None,
            account: None,
            model: None,
            effort: None,
            cwd: None,
            container: None,
            sandbox: None,
            resume_count: 0,
            resume_of: None,
            lease_sweep_id: None,
        }),
        worktree: None,
        checkpoint_phase: Some("builder".to_string()),
        log_path: None,
        role: None,
        timeout_remaining_secs: None,
        holds_issue_creation_mutex: false,
    }
}

// ============================================================================
// The classification the manifest records
// ============================================================================

#[test]
fn the_snapshot_is_classified_young_unresumable_or_resume() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let old = manifest_item(&cand(dir.path(), "old", 1, 900), now, 300);
    assert_eq!((old.disposition, old.reason), (Disposition::Resume, None));

    let young = manifest_item(&cand(dir.path(), "young", 2, 10), now, 300);
    assert_eq!(young.disposition, Disposition::Requeue);
    assert_eq!(young.reason.as_deref(), Some(pause_classify::REASON_YOUNG));

    let mut no_session = cand(dir.path(), "nosess", 3, 900);
    no_session.resume_handle.as_mut().unwrap().session_id = None;
    let no_session = manifest_item(&no_session, now, 300);
    assert_eq!(no_session.reason.as_deref(), Some(pause_classify::REASON_NOT_RESUMABLE));

    // Dispatched before #10830: no item id, so it cannot be asked to pause.
    let mut no_dir = cand(dir.path(), "nodir", 4, 900);
    no_dir.pause_dir = None;
    let no_dir = manifest_item(&no_dir, now, 300);
    assert_eq!(no_dir.reason.as_deref(), Some(pause_classify::REASON_NOT_RESUMABLE));
}

/// Judge finding on #10864: a start time in the future (clock skew) is
/// unknown. It must not read as "just started" and reset a long-running agent.
#[test]
fn a_start_time_in_the_future_is_unknown_not_young() {
    let dir = tempfile::tempdir().unwrap();
    let skewed = cand(dir.path(), "skew", 1, -3600);
    assert_eq!(skewed.agent_age_secs(Utc::now()), None);
    let item = manifest_item(&skewed, Utc::now(), 300);
    assert_eq!(item.disposition, Disposition::Resume, "never `young-agent-reset`");
}

// ============================================================================
// Config
// ============================================================================

#[test]
fn the_default_budgets_fit_under_two_thirds_of_the_lease_ttl() {
    let t = PauseRollTuning::defaults(Duration::from_secs(900));
    assert_eq!(
        (
            t.pause_budget.as_secs(),
            t.verify_probation.as_secs(),
            t.resume_budget.as_secs()
        ),
        (120, 90, 120)
    );
    t.validate().unwrap();
}

#[test]
fn validation_rejects_a_sum_that_reaches_two_thirds_of_the_lease_ttl() {
    let mut t = PauseRollTuning::defaults(Duration::from_secs(900));
    // 2/3 of 900 is 600: 599 passes, 600 is rejected.
    t.pause_budget = Duration::from_secs(389);
    t.validate().unwrap();
    t.pause_budget = Duration::from_secs(390);
    let why = t.validate().unwrap_err();
    assert!(why.contains("600s") && why.contains("lease TTL"), "{why}");
    t.pause_budget = Duration::ZERO;
    assert!(t.validate().unwrap_err().contains("positive"));
}

#[test]
#[serial(loom_auto_update_env)]
fn resolve_reads_the_config_keys_and_rejects_an_invalid_combination() {
    for var in [PAUSE_BUDGET_ENV, VERIFY_PROBATION_ENV, RESUME_BUDGET_ENV] {
        std::env::remove_var(var);
    }
    std::env::remove_var(crate::claim_reconciliation::LEASE_TTL_MINUTES_ENV);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    let write = |body: &str| std::fs::write(tmp.path().join(".loom/config.json"), body).unwrap();

    write(
        r#"{"autonomous":{"autoUpdate":{"pauseRoll":{"pauseBudgetSecs":60,"verifyProbationSecs":45,"resumeBudgetSecs":30}}}}"#,
    );
    let (t, rejected) = PauseRollTuning::resolve(tmp.path());
    assert_eq!(rejected, None);
    assert_eq!(
        (
            t.pause_budget.as_secs(),
            t.verify_probation.as_secs(),
            t.resume_budget.as_secs()
        ),
        (60, 45, 30)
    );

    write(
        r#"{"autonomous":{"autoUpdate":{"pauseRoll":{"pauseBudgetSecs":500,"minResumableAgeSecs":7}}}}"#,
    );
    let (t, rejected) = PauseRollTuning::resolve(tmp.path());
    assert!(rejected.unwrap().contains("REJECTED"));
    assert_eq!(t.pause_budget.as_secs(), DEFAULT_PAUSE_BUDGET_SECS, "the defaults are used");
    assert_eq!(t.min_resumable_age_secs, 7, "an unrelated valid knob is kept");

    // Env wins over config.
    std::env::set_var(PAUSE_BUDGET_ENV, "100");
    let (t, rejected) = PauseRollTuning::resolve(tmp.path());
    std::env::remove_var(PAUSE_BUDGET_ENV);
    assert_eq!((t.pause_budget.as_secs(), rejected), (100, None));
}

// ============================================================================
// H4 with a scripted host
// ============================================================================

/// AC: a manifest write failure in step 3 aborts the pause with nothing
/// signalled and dispatch resumed.
#[test]
fn a_manifest_write_failure_aborts_the_pause_with_nothing_signalled() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let mut plan = begin(&drain, dir.path(), tuning(2000));
    // The manifest's parent is a regular file: the write cannot succeed.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    plan.manifest_path = blocker.join(pause_manifest::MANIFEST_FILE);
    let host = Arc::new(FakeHost {
        cands: vec![
            cand(dir.path(), "old", 1, 900),
            cand(dir.path(), "young", 2, 5),
        ],
        ..FakeHost::default()
    });

    let outcome = run_h4(&drain, host.clone(), &plan);

    assert!(
        matches!(&outcome, H4Outcome::Failed(why) if why.contains("PAUSE FAILED")),
        "{outcome:?}"
    );
    assert!(host.torn.lock().unwrap().is_empty(), "nothing was stopped");
    assert!(host.held.lock().unwrap().is_empty(), "nothing was taken from the reaper");
    for c in &host.cands {
        assert!(
            !roll_pause::is_requested(c.pause_dir.as_ref().unwrap()),
            "no pause request was raised"
        );
    }
    assert!(!drain.is_draining(), "dispatch resumed");
    assert!(drain.snapshot().note.unwrap().contains("pause-failed"));
    assert!(host
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|(t, _)| t == "daemon.roll.pause_failed"));
}

/// AC: a forge write that runs past the budget is deferred (`status =
/// planned`) and does not delay the exit.
#[test]
fn a_forge_write_past_the_budget_is_deferred_and_does_not_delay_the_exit() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(300));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "young", 2, 5)],
        requeue_delay: Duration::from_secs(30),
        ..FakeHost::default()
    });

    let started = Instant::now();
    let outcome = run_h4(&drain, host.clone(), &plan);
    let took = started.elapsed();

    assert!(matches!(outcome, H4Outcome::Paused { .. }), "{outcome:?}");
    assert!(took < Duration::from_secs(10), "a slow forge blocked the roll for {took:?}");
    let m = load(&plan);
    assert_eq!(m.phase, Phase::Paused);
    let young = item(&m, "young");
    assert_eq!(young.status, ItemStatus::Planned, "left for the next start to finish");
    assert_eq!(young.disposition, Disposition::Requeue);
    assert!(young.stopped_at.is_some(), "its tree was still stopped");
    let events = host.item_events();
    assert_eq!(events[0]["forge"], "deferred");
    assert_eq!(drain.snapshot().pause.unwrap().deferred_forge_writes, 1);
}

/// A forge write that fails outright is deferred the same way.
#[test]
fn a_failed_forge_write_leaves_the_item_planned() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(300));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "young", 2, 5)],
        requeue_fails: true,
        ..FakeHost::default()
    });
    assert!(matches!(run_h4(&drain, host, &plan), H4Outcome::Paused { .. }));
    assert_eq!(item(&load(&plan), "young").status, ItemStatus::Planned);
}

/// AC: a process death inside H4 leaves a manifest the next start can finish.
/// The manifest on disk at the moment the first tree is stopped already says
/// `phase = pausing` and names every in-flight agent with its disposition.
#[test]
fn the_manifest_on_disk_before_the_first_stop_names_every_agent() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(400));
    let seen: Arc<Mutex<Option<PauseManifest>>> = Arc::new(Mutex::new(None));
    let (seen_in, path) = (seen.clone(), plan.manifest_path.clone());
    let host = Arc::new(FakeHost {
        cands: vec![
            cand(dir.path(), "old", 1, 900),
            cand(dir.path(), "young", 2, 5),
        ],
        on_teardown: Some(Box::new(move |_id| {
            let mut slot = seen_in.lock().unwrap();
            if slot.is_none() {
                // What a process killed right here would leave behind.
                if let LoadOutcome::Loaded(m) = pause_manifest::load(&path, Utc::now()) {
                    *slot = Some(m);
                }
            }
        })),
        ..FakeHost::default()
    });

    assert!(matches!(run_h4(&drain, host, &plan), H4Outcome::Paused { .. }));

    let m = seen
        .lock()
        .unwrap()
        .clone()
        .expect("a manifest existed before the first stop");
    assert_eq!(m.phase, Phase::Pausing);
    assert_eq!(m.roll.to_version, "0.19.900");
    assert_eq!(m.roll.target_source, Some(TargetSource::Floor));
    assert_eq!(m.roll.pause_budget_secs, Some(0));
    assert_eq!(m.roll.max_age_secs, 900);
    assert_eq!(m.written_by.version, "0.19.887");
    let ids: BTreeSet<&str> = m.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, BTreeSet::from(["old", "young"]));
    assert_eq!(item(&m, "old").disposition, Disposition::Resume);
    assert_eq!(item(&m, "young").disposition, Disposition::Requeue);
    assert_eq!(item(&m, "young").reason.as_deref(), Some("young-agent-reset"));
    // Nothing was marked stopped before it was recorded.
    assert!(m.items.iter().all(|i| i.stopped_at.is_none()));
}

/// An agent that ends by itself before a safe point is `exited`, not paused
/// and not requeued: the existing crash path decides at the next start.
#[test]
fn an_agent_that_exits_by_itself_is_recorded_as_exited() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(2000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    host.dead.lock().unwrap().insert("old".to_string());

    assert!(matches!(run_h4(&drain, host.clone(), &plan), H4Outcome::Paused { .. }));

    let m = load(&plan);
    assert_eq!(item(&m, "old").status, ItemStatus::Exited);
    assert!(host.torn.lock().unwrap().is_empty());
    assert!(host.requeued.lock().unwrap().is_empty());
    assert_eq!(drain.snapshot().pause.unwrap().exited, 1);
}

// ---- The three operator-interplay rules -------------------------------------

/// Rule 1: a relaunch operator request before anything is stopped promotes the
/// drain to `Operator`; the pause stands down (requests withdrawn, manifest
/// deleted, nothing stopped).
#[test]
fn rule_1_an_operator_request_before_anything_is_stopped_promotes_and_the_pause_stands_down() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    drain.set_roll_target(Some("v0.19.900@abcd".to_string()));
    // One resumable agent that never reaches a safe point: the pause sits in
    // step 5 with nothing stopped.
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();

    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert_eq!(drain.snapshot().pause.unwrap().step, 5);

    match drain.begin(Duration::from_secs(1800), false, false) {
        DrainBegin::AlreadyDraining {
            origin_promoted, ..
        } => assert!(origin_promoted, "nothing stopped yet: promoted (#9588)"),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }

    assert_eq!(worker.join().unwrap(), H4Outcome::StoodDown { promoted: true });
    let snap = drain.snapshot();
    assert_eq!(snap.origin, DrainOrigin::Operator);
    assert!(drain.is_draining(), "the operator drain keeps dispatch paused");
    assert_eq!(snap.roll_target, None, "the target label is cleared");
    assert!(snap.pause.is_none());
    assert!(host.torn.lock().unwrap().is_empty(), "stops nothing");
    assert!(!roll_pause::is_requested(&pause_dir), "pause request withdrawn");
    assert!(!plan.manifest_path.exists(), "manifest deleted");
    assert!(host.held.lock().unwrap().is_empty(), "handed back to the reaper");
}

/// Rule 2: once something has been stopped, a relaunch operator request is
/// acked `AlreadyDraining` with no promotion, `--abort-drain` is refused with
/// a message naming the step, and the roll completes.
#[test]
fn rule_2_once_something_is_stopped_requests_do_not_promote_abort_is_refused_and_the_roll_completes(
) {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(3000));
    // The young agent is torn down in step 4 (the commit); the old one never
    // reaches a safe point, so the pause then sits in step 5.
    let host = Arc::new(FakeHost {
        cands: vec![
            cand(dir.path(), "young", 2, 5),
            cand(dir.path(), "old", 1, 900),
        ],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[1].pause_dir.clone().unwrap();

    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert!(drain.snapshot().pause.unwrap().stopped);

    match drain.begin(Duration::from_secs(1800), false, false) {
        DrainBegin::AlreadyDraining {
            origin_promoted,
            active_then_exit,
            ..
        } => {
            assert!(!origin_promoted, "work was stopped: no promotion");
            assert!(!active_then_exit);
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert_eq!(drain.snapshot().origin, DrainOrigin::PauseRoll);
    match drain.abort_checked() {
        AbortOutcome::Refused(why) => {
            assert!(why.contains("H4 step 5"), "{why}");
            assert!(why.contains("already stopped"), "{why}");
        }
        other => panic!("expected the abort to be refused, got {other:?}"),
    }
    assert!(!drain.abort(), "the bool form refuses too");
    assert!(!drain.abort_pause_roll(), "and it is too late to supersede");
    assert!(drain.is_draining());

    let outcome = worker.join().unwrap();
    assert!(
        matches!(
            outcome,
            H4Outcome::Paused {
                then_exit: false,
                ..
            }
        ),
        "the roll completes: {outcome:?}"
    );
    let m = load(&plan);
    assert_eq!(m.phase, Phase::Paused);
    assert_eq!(item(&m, "old").reason.as_deref(), Some(REASON_BUDGET_MISSED));
    // #11049: the manifest says why. The fake agent never ran the hook.
    let miss = item(&m, "old").safe_point_miss.clone().unwrap();
    assert_eq!(miss.cause, "no-hook", "{miss:?}");
}

/// Rule 3: a then-exit request always wins. After something is stopped it is
/// recorded (no promotion), the pause still writes `phase = paused`, and the
/// daemon exits WITHOUT relaunch; the manifest stays for the next start.
#[test]
fn rule_3_a_then_exit_wins_and_the_daemon_exits_without_relaunch_leaving_phase_paused() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("autonomy-desired");
    std::fs::write(&marker, "started_at=2026-10-07T00:00:00Z\n").unwrap();
    let drain = Arc::new(DrainState::new().with_stop_marker(marker.clone()));
    let plan = begin(&drain, dir.path(), tuning(1500));
    let host = Arc::new(FakeHost {
        cands: vec![
            cand(dir.path(), "young", 2, 5),
            cand(dir.path(), "old", 1, 900),
        ],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[1].pause_dir.clone().unwrap();

    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));

    match drain.begin(Duration::from_secs(1800), false, true) {
        DrainBegin::AlreadyDraining {
            origin_promoted,
            escalated,
            active_then_exit,
            ..
        } => {
            assert!(escalated && active_then_exit, "the then-exit is recorded");
            assert!(!origin_promoted, "work was stopped: the pause keeps the drain");
        }
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert!(
        crate::operator_stop::is_recorded(&marker),
        "operator-stop record written (#4343)"
    );

    let outcome = worker.join().unwrap();
    let H4Outcome::Paused { then_exit, .. } = outcome else {
        panic!("expected Paused, got {outcome:?}");
    };
    assert!(then_exit, "the caller exits to stay down");
    assert_eq!(crate::ipc::drain_exit_code(then_exit), crate::ipc::EXIT_SHUTDOWN);
    assert_eq!(load(&plan).phase, Phase::Paused, "the manifest stays for the next start");
}

/// Rule 3, before step 4: rule 1 applies. The then-exit promotes the drain and
/// the pause stands down; the operator's teardown drain then owns the exit.
#[test]
fn rule_3_a_then_exit_before_anything_is_stopped_promotes_like_rule_1() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    // Still waiting for a Pending dispatch to settle (step 1).
    host.pending.store(1, Ordering::SeqCst);

    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || {
        drain.snapshot().pause.is_some_and(|p| p.step == 1)
    }));

    match drain.begin(Duration::from_secs(1800), false, true) {
        DrainBegin::AlreadyDraining {
            origin_promoted,
            active_then_exit,
            ..
        } => assert!(origin_promoted && active_then_exit),
        other => panic!("expected AlreadyDraining, got {other:?}"),
    }
    assert_eq!(worker.join().unwrap(), H4Outcome::StoodDown { promoted: true });
    assert!(host.torn.lock().unwrap().is_empty());
    assert!(!plan.manifest_path.exists());
    assert!(drain.snapshot().then_exit && drain.is_draining());
}

/// Design §7 failure edges: `--abort-drain` is honoured in step 5 while
/// nothing is stopped yet. The pause requests are withdrawn (a parked hook
/// then returns allow) and nothing is stopped.
#[test]
fn an_abort_in_step_5_with_nothing_stopped_is_honoured_and_withdraws_the_requests() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();

    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));

    assert_eq!(drain.abort_checked(), AbortOutcome::Aborted);
    assert_eq!(worker.join().unwrap(), H4Outcome::StoodDown { promoted: false });
    assert!(!drain.is_draining(), "dispatch resumed");
    assert!(drain
        .snapshot()
        .note
        .unwrap()
        .contains("before it stopped any agent"));
    assert!(host.torn.lock().unwrap().is_empty());
    assert!(!roll_pause::is_requested(&pause_dir));
    assert!(!plan.manifest_path.exists());
}

/// Supersede (#8514) works until the pause commits: the auto-updater can end
/// its own uncommitted pause roll, and the pause stands down.
#[test]
fn a_supersede_before_the_commit_ends_the_pause_roll() {
    let dir = tempfile::tempdir().unwrap();
    let drain = Arc::new(DrainState::new());
    let plan = begin(&drain, dir.path(), tuning(30_000));
    let host = Arc::new(FakeHost {
        cands: vec![cand(dir.path(), "old", 1, 900)],
        ..FakeHost::default()
    });
    let pause_dir = host.cands[0].pause_dir.clone().unwrap();
    let (d, h, p) = (drain.clone(), host.clone(), plan.clone());
    let worker = std::thread::spawn(move || run_h4(&d, h, &p));
    assert!(test_support::wait_for_condition(20_000, || roll_pause::is_requested(
        &pause_dir
    )));
    assert!(drain.abort_pause_roll());
    assert_eq!(worker.join().unwrap(), H4Outcome::StoodDown { promoted: false });
    assert!(host.torn.lock().unwrap().is_empty());
}

// ============================================================================
// H3: start_pause_roll
// ============================================================================

/// H3 never pauses on top of a manifest that is still live, and never falls
/// back to a drain: the roll is refused and dispatch is untouched.
#[tokio::test]
#[serial(loom_daemon_supervisor)]
async fn h3_refuses_while_the_previous_rolls_manifest_is_still_live() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
    std::env::set_var(crate::auto_update::AUTO_UPDATE_STATE_DIR_ENV, root.join("state"));
    // A finished pause from a moment ago.
    {
        let drain = DrainState::new();
        let plan = begin(&drain, &root, tuning(200));
        let host = Arc::new(FakeHost::default());
        assert!(matches!(run_h4(&drain, host, &plan), H4Outcome::Paused { .. }));
        assert_eq!(load(&plan).phase, Phase::Paused);
    }
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    let drain = Arc::new(DrainState::new());

    let started = start_pause_roll(&drain, &pool, &root, &bus, &target());

    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    std::env::remove_var(crate::auto_update::AUTO_UPDATE_STATE_DIR_ENV);
    assert!(!started);
    assert!(!drain.is_draining(), "nothing was paused, and no drain fallback");
    assert_eq!(drain.generation(), 0);
}

// ============================================================================
// Integration: real process trees, a real registry, a fake gh
// ============================================================================

/// A [`PauseHost`] over one real sweep registry, with real process trees.
pub(super) struct RegistryHost {
    pub(super) registry: Arc<Mutex<SweepRegistry>>,
    pub(super) root: PathBuf,
    pub(super) events: Mutex<Vec<(String, serde_json::Value)>>,
    pub(super) leases: Mutex<Vec<String>>,
}

impl PauseHost for RegistryHost {
    fn close_dispatch(&self, closed: bool, run: &str) {
        self.registry.lock().unwrap().close_for_roll(run, closed);
    }
    fn pending_dispatches(&self) -> usize {
        self.registry.lock().unwrap().mid_spawn_dispatches()
    }
    fn snapshot(&self) -> Vec<Candidate> {
        let mut cands = sweep_candidates(&self.registry.lock().unwrap(), &self.root);
        host::stamp_proc_starts(&mut cands);
        cands
    }
    fn hold(&self, c: &Candidate, held: bool, run: &str) {
        if held {
            roll_pause::hold::hold(&c.id, run);
        } else {
            roll_pause::hold::release(&c.id, run);
        }
    }
    fn is_alive(&self, c: &Candidate) -> bool {
        c.pid.is_some_and(teardown::pid_running)
    }
    fn teardown(&self, c: &Candidate) -> TeardownReport {
        teardown::teardown_tree(&c.tree_spec(), Duration::from_millis(300))
    }
    fn force_kill(&self, c: &Candidate) -> bool {
        teardown::force_kill_group(&c.tree_spec())
    }
    fn refresh_lease(&self, c: &Candidate, _timeout: Duration) -> Result<(), String> {
        self.leases.lock().unwrap().push(c.id.clone());
        Ok(())
    }
    fn requeue(&self, c: &Candidate, notice: &RollRequeueNotice) -> Result<(), String> {
        host::requeue_off_the_registry_lock(&self.registry, c.issue.unwrap(), notice)
    }
    fn emit(&self, topic: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((topic.to_string(), payload));
    }
}

/// A fake agent: a process group whose child has left it with `setsid()`.
pub(super) struct FakeAgent {
    pub(super) child: std::process::Child,
    pub(super) setsid_pid: u32,
}

impl FakeAgent {
    pub(super) fn spawn(dir: &Path, name: &str) -> Self {
        let pidfile = dir.join(format!("{name}.setsid.pid"));
        let script = format!(
            "perl -e 'use POSIX; my $p = fork(); if ($p == 0) {{ POSIX::setsid(); \
             open(my $f, \">\", \"{}\"); print $f $$; close($f); sleep 300; exit 0 }} sleep 300'",
            pidfile.display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script).process_group(0);
        let child = cmd.spawn().unwrap();
        let setsid_pid = test_support::read_pid_file(&pidfile, 30_000)
            .expect("the setsid'd child never started");
        Self { child, setsid_pid }
    }
    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }
    pub(super) fn gone(&mut self) -> bool {
        // Reap the direct child so its pid stops reading as alive.
        let _ = self.child.try_wait();
        test_support::wait_until_dead(self.setsid_pid, 10_000)
            && test_support::wait_for_condition(10_000, || {
                matches!(self.child.try_wait(), Ok(Some(_)))
            })
    }
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        crate::sweep_registry::reaper::send_group_signal(self.pid(), libc::SIGKILL);
        crate::sweep_registry::reaper::send_signal(self.setsid_pid, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// Register `agent` as issue `issue`'s running sweep: a registry entry, a
/// claim lock stamped with its pause-and-roll identity (as dispatch does), a
/// checkpoint, and a session `age_secs` old. Returns `(sweep_id, pause_dir)`.
pub(super) fn register(
    registry: &mut SweepRegistry,
    root: &Path,
    agent: &FakeAgent,
    issue: u32,
    age_secs: i64,
) -> (String, PathBuf) {
    let sweep_id =
        test_support::insert_running_with_pid_at(registry, issue, 1, agent.pid(), Utc::now());
    let lock = test_support::write_lock_owner(registry, issue, &sweep_id, agent.pid());
    let session =
        crate::sweep_registry::resume_handle::DispatchSession::new(&sweep_id, root, Some("claude"))
            .unwrap();
    registry.stamp_resume_handle_in_lock(issue, &session, Some("opus"), Some("high"));
    // Backdate the session's first start and record the process group.
    let owner_path = lock.join("owner.json");
    let mut owner: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&owner_path).unwrap()).unwrap();
    owner["agent_started_at"] =
        serde_json::json!((Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339());
    owner["pgid"] = serde_json::json!(agent.pid());
    std::fs::write(&owner_path, serde_json::to_string_pretty(&owner).unwrap()).unwrap();
    test_support::write_checkpoint(registry, issue, "builder");
    (sweep_id, session.item_dir())
}

/// AC: with fake agents, one item is paused, a young one is reset
/// (`young-agent-reset`), and one misses the budget and is requeued
/// (`pause-budget-missed`). After H4 no process from any item's tree is alive,
/// including a `setsid`'d child, and every paused item's lock, journal entry
/// and checkpoint is still present — even after a reaper tick.
#[test]
fn h4_pauses_resets_and_requeues_real_agents_and_keeps_the_paused_items_recovery_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let gh_log = root.join("gh.log");
    let gh = root.join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n\
             if [ \"$1\" = issue ] && [ \"$2\" = view ]; then echo false; fi\nexit 0\n",
            gh_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(root.clone());
    config.gh_bin = Some(gh);
    config.journal_path = Some(root.join("sweeps.json"));
    let mut registry = SweepRegistry::new(config);

    let mut paused = FakeAgent::spawn(&root, "paused");
    let mut young = FakeAgent::spawn(&root, "young");
    let mut missed = FakeAgent::spawn(&root, "missed");
    let (paused_id, paused_dir) = register(&mut registry, &root, &paused, 101, 900);
    let (young_id, young_dir) = register(&mut registry, &root, &young, 102, 5);
    let (missed_id, missed_dir) = register(&mut registry, &root, &missed, 103, 900);
    test_support::write_journal_entry(&registry, "o/r", 101, paused.pid());

    // Only the first agent's hook answers the pause request.
    let hook = spawn_parking_agent(paused_dir.clone());

    let host = Arc::new(RegistryHost {
        registry: Arc::new(Mutex::new(registry)),
        root: root.clone(),
        events: Mutex::default(),
        leases: Mutex::default(),
    });
    let drain = DrainState::new();
    let mut plan = begin(&drain, &root, tuning(6000));
    plan.tuning.forge_floor = Duration::from_secs(20);

    let outcome = run_h4(&drain, host.clone(), &plan);

    let H4Outcome::Paused {
        manifest_id,
        then_exit,
    } = outcome
    else {
        panic!("expected Paused, got {outcome:?}");
    };
    assert!(!then_exit);

    // The manifest: one paused, one reset, one budget miss.
    let m = load(&plan);
    assert_eq!((m.phase.clone(), m.manifest_id.as_str()), (Phase::Paused, manifest_id.as_str()));
    assert!(m.roll.pause_completed_at.is_some());
    let p = item(&m, &paused_id);
    assert_eq!(
        (p.disposition.clone(), p.status.clone()),
        (Disposition::Resume, ItemStatus::Paused)
    );
    let sp = p.safe_point.as_ref().expect("the safe point is recorded");
    assert_eq!(sp.parked_tool.as_deref(), Some("Bash"));
    assert_eq!(sp.parked_summary.as_deref(), Some("cargo test"));
    assert!(p.stopped_at.is_some() && p.lease_refreshed_at.is_some());
    assert_eq!(p.checkpoint_phase.as_deref(), Some("builder"));
    assert!(p.resume_handle.as_ref().unwrap().session_id.is_some());
    assert_eq!(*host.leases.lock().unwrap(), vec![paused_id.clone()]);

    let y = item(&m, &young_id);
    assert_eq!(y.reason.as_deref(), Some("young-agent-reset"));
    assert_eq!(
        (y.disposition.clone(), y.status.clone()),
        (Disposition::Requeue, ItemStatus::Requeued)
    );
    let x = item(&m, &missed_id);
    assert_eq!(x.reason.as_deref(), Some("pause-budget-missed"));
    assert_eq!(
        (x.disposition.clone(), x.status.clone()),
        (Disposition::Requeue, ItemStatus::Requeued)
    );

    // No process from any tree is alive, the setsid'd children included.
    assert!(paused.gone(), "the paused agent's tree is still alive");
    assert!(young.gone(), "the reset agent's tree is still alive");
    assert!(missed.gone(), "the budget-missed agent's tree is still alive");

    // The paused item's lock, journal entry and checkpoint are untouched, and
    // stay so across a reaper tick (which would otherwise release the lock of
    // a sweep whose child just died).
    let lock = |issue: u32| root.join(format!(".loom/locks/issue-{issue}/owner.json"));
    let checkpoint = |issue: u32| root.join(format!(".loom/sweep-checkpoint/issue-{issue}.json"));
    host.registry.lock().unwrap().reap_once();
    assert!(lock(101).is_file(), "H4 must never delete a paused item's lock");
    assert!(checkpoint(101).is_file(), "nor its checkpoint");
    let journal = std::fs::read_to_string(root.join("sweeps.json")).unwrap();
    assert!(
        journal.contains("\"issue\": 101") || journal.contains("\"issue\":101"),
        "{journal}"
    );
    // The requeued items keep theirs too: a write left `planned`, or a binary
    // that cannot read the manifest, recovers from them (design §8).
    assert!(lock(102).is_file() && lock(103).is_file());
    assert!(checkpoint(102).is_file() && checkpoint(103).is_file());

    // Each requeue: the label restored and exactly one comment, with the roll,
    // the reason, the phase and the worktree state.
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap();
    for (issue, reason) in [(102, "young-agent-reset"), (103, "pause-budget-missed")] {
        // The body is multi-line, so count the call by its first line and
        // look for the rest in the log as a whole.
        let comments = gh_calls
            .lines()
            .filter(|l| l.starts_with(&format!("issue comment {issue} ")))
            .count();
        assert_eq!(comments, 1, "one comment for #{issue}: {gh_calls}");
        assert!(gh_calls.contains(&format!("`{reason}`")), "{gh_calls}");
    }
    assert!(gh_calls.contains("`0.19.887` → `0.19.900`"), "{gh_calls}");
    assert!(gh_calls.contains("**Phase reached:** builder"), "{gh_calls}");
    assert!(!gh_calls.contains("issue comment 101"), "a paused item is not requeued");
    assert!(gh_calls.contains("loom:issue"), "the label was restored: {gh_calls}");

    // Telemetry: one `daemon.roll.item` per item, budgets and durations
    // included, and the per-reason counter in status.
    let events = host.events.lock().unwrap();
    let items: Vec<&serde_json::Value> = events
        .iter()
        .filter(|(t, _)| t == "daemon.roll.item")
        .map(|(_, p)| p)
        .collect();
    assert_eq!(items.len(), 3);
    let by_id = |id: &str| *items.iter().find(|p| p["item_id"] == id).unwrap();
    let pe = by_id(&paused_id);
    assert_eq!(
        (pe["status"].as_str(), pe["disposition"].as_str()),
        (Some("paused"), Some("resume"))
    );
    assert_eq!(pe["runtime"], "claude");
    assert_eq!(pe["pause_budget_secs"], 6);
    assert_eq!(pe["target_source"], "floor");
    assert!(pe["safe_point_wait_ms"].is_u64() && pe["teardown_ms"].is_u64(), "{pe}");
    assert_eq!(by_id(&young_id)["reason"], "young-agent-reset");
    assert_eq!(by_id(&missed_id)["reason"], "pause-budget-missed");
    assert_eq!(by_id(&missed_id)["safe_point_miss"], "no-hook", "#11049");
    assert!(by_id(&paused_id)["safe_point_miss"].is_null());
    assert!(events.iter().any(|(t, _)| t == "daemon.roll.paused"));
    let status = drain.snapshot().pause.unwrap();
    assert_eq!((status.items, status.paused, status.step), (3, 1, 10));
    assert_eq!(status.requeued_by_reason["young-agent-reset"], 1);
    assert_eq!(status.requeued_by_reason["pause-budget-missed"], 1);
    assert!(status.stop_secs.is_some() && status.settle_secs.is_some());

    // Every pause request is withdrawn. The safe point H5 reads is the
    // manifest's (asserted above). The record on disk answers this request
    // only: here the stand-in hook outlives the "stopped" tree, sees the
    // request go and releases its call, which removes the record.
    for d in [&paused_dir, &young_dir, &missed_dir] {
        assert!(!roll_pause::is_requested(d));
    }
    hook.join().unwrap();
    assert!(roll_pause::read_safe_point(&paused_dir).is_none());
    // The gate this pause closed stays closed: the daemon exits next.
    assert!(host.registry.lock().unwrap().closed_for_roll());
    for id in [&paused_id, &young_id, &missed_id] {
        roll_pause::hold::release(id, &manifest_id);
    }
}
