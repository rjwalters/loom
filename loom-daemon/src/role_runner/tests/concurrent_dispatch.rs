//! Tests for concurrent per-`(repository, role)` dispatch (#9391).
//!
//! Most tests inject a narrow per-root decision (`admit_root_tick` against the
//! root's own config) so they do not depend on the `LOOM_ROLE_RUNNER` env
//! tier; the one test that drives the production decision is `#[serial]`,
//! like the `decide_root_tick_*` tests it mirrors. Tests that reap through
//! the #4349 log dedup write the process-global tick ring, so they hold
//! `#[serial(role_tick_ring)]`.

use super::*;
use serial_test::serial;
use std::sync::atomic::AtomicUsize;
use std::sync::Condvar;

// -- fixtures ---------------------------------------------------------------

fn spec(name: &'static str) -> RoleSpec {
    *DEFAULT_ROLES
        .iter()
        .find(|s| s.name == name)
        .expect("role is in DEFAULT_ROLES")
}

fn workspace(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(tmp.path().join(".loom").join("config.json"), config).unwrap();
    tmp
}

const ENABLED: &str = r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Admission only, against each root's own config — no enablement/shard
/// prefix, so no env dependence.
fn admit_only(role: &'static str) -> DecideFn {
    Box::new(move |root, in_progress, _| {
        admit_root_tick(
            root,
            role,
            format!("/loom:{role}"),
            in_progress,
            &read_role_runner_config(root),
        )
    })
}

fn queue_has_work() -> QueueProbe {
    Arc::new(|_, _| Ok(true))
}

fn dispatcher(role: &'static str, factory: RunnerFactory, decide: DecideFn) -> RoleDispatcher {
    RoleDispatcher::with_decide(
        spec(role),
        Duration::from_secs(300),
        factory,
        queue_has_work(),
        None,
        decide,
    )
}

fn join_one(
    rt: &tokio::runtime::Runtime,
    d: &mut RoleDispatcher,
) -> Result<(Id, FinishedRun), JoinError> {
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), d.join_next()).await })
        .expect("a run finished within 30s")
        .expect("a run was in flight")
}

/// A runner that returns a fixed outcome.
struct FixedRunner(RoleTickOutcome, Arc<AtomicUsize>);

impl RoleInvocationRunner for FixedRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.clone()
    }
}

/// An N-party latch: each invocation arrives, then waits until all N have
/// arrived. Under the pre-#9391 serial `await` the first arrival is alone,
/// so it times out and reports `Failure`.
struct LatchRunner {
    latch: Arc<(Mutex<usize>, Condvar)>,
    parties: usize,
    timeout: Duration,
}

impl RoleInvocationRunner for LatchRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        let (lock, cvar) = &*self.latch;
        let mut arrived = lock.lock().unwrap();
        *arrived += 1;
        cvar.notify_all();
        let (arrived, timeout) = cvar
            .wait_timeout_while(arrived, self.timeout, |n| *n < self.parties)
            .unwrap();
        if timeout.timed_out() {
            RoleTickOutcome::Failure(format!(
                "latch timed out with {} of {} arrived",
                *arrived, self.parties
            ))
        } else {
            RoleTickOutcome::Success
        }
    }
}

/// A gate that blocked runners wait on until `open`.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn wait(&self) {
        let (lock, cvar) = &*self.0;
        let open = lock.lock().unwrap();
        let _open = cvar
            .wait_timeout_while(open, Duration::from_secs(30), |o| !*o)
            .unwrap();
    }
    fn open(&self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

/// Blocks on `gate` when its root is in `blocked`, otherwise returns at once.
struct GatedRunner {
    root: PathBuf,
    blocked: Vec<PathBuf>,
    gate: Gate,
}

impl RoleInvocationRunner for GatedRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        if self.blocked.contains(&self.root) {
            self.gate.wait();
        }
        RoleTickOutcome::Success
    }
}

fn gated_factory(blocked: Vec<PathBuf>, gate: Gate) -> RunnerFactory {
    Arc::new(move |root| {
        Box::new(GatedRunner {
            root,
            blocked: blocked.clone(),
            gate: gate.clone(),
        })
    })
}

// -- AC1: concurrent across repositories -----------------------------------

fn latch_factory(parties: usize, timeout: Duration) -> RunnerFactory {
    let latch = Arc::new((Mutex::new(0usize), Condvar::new()));
    Arc::new(move |_| {
        Box::new(LatchRunner {
            latch: Arc::clone(&latch),
            parties,
            timeout,
        })
    })
}

/// AC1: three enabled roots' judge runs are all in flight at once. The runner
/// is a 3-party latch, so a serial loop (await each root before the next)
/// would leave the first run alone until its latch timed out — see the
/// control test below.
#[test]
fn ac1_one_tick_runs_three_repositories_concurrently() {
    let rt = runtime();
    let _enter = rt.enter();
    let roots: Vec<tempfile::TempDir> = (0..3).map(|_| workspace(ENABLED)).collect();
    let factory = latch_factory(3, Duration::from_secs(10));
    let mut d = dispatcher("judge", factory, admit_only("judge"));
    let in_progress = new_in_progress_guard();

    let report =
        d.dispatch_tick(roots.iter().map(|t| t.path().to_path_buf()).collect(), &in_progress);
    assert_eq!(report.spawned.len(), 3, "all three roots admitted in one tick");
    assert_eq!(d.in_flight_len(), 3);

    for _ in 0..3 {
        let (_, run) = join_one(&rt, &mut d).unwrap();
        assert_eq!(
            run.outcome,
            RoleTickOutcome::Success,
            "every run must meet the other two at the latch — they ran concurrently"
        );
    }
    assert_eq!(active_run_count(&in_progress), 0, "every guard released on completion");
}

/// Control for AC1: the same latch driven in the pre-#9391 serial shape (each
/// root's run awaited before the next starts) times out, so the AC1 test
/// really does distinguish concurrent from serial dispatch.
#[test]
fn ac1_control_the_latch_times_out_under_serial_dispatch() {
    let rt = runtime();
    let _enter = rt.enter();
    let roots: Vec<tempfile::TempDir> = (0..3).map(|_| workspace(ENABLED)).collect();
    let factory = latch_factory(3, Duration::from_millis(300));
    let mut d = dispatcher("judge", factory, admit_only("judge"));
    let in_progress = new_in_progress_guard();
    let mut outcomes = Vec::new();
    for root in &roots {
        d.dispatch_tick(vec![root.path().to_path_buf()], &in_progress);
        outcomes.push(join_one(&rt, &mut d).unwrap().1.outcome);
    }
    assert!(
        matches!(&outcomes[0], RoleTickOutcome::Failure(m) if m.contains("latch timed out")),
        "serial dispatch leaves the first run alone at the latch: {outcomes:?}"
    );
}

// -- AC2: one instance per (repository, role) -------------------------------

/// AC2, through the production decision: while root A's judge run is still
/// in flight, the next tick refuses A as in progress and admits B and C again.
#[test]
#[serial]
fn ac2_a_root_still_in_flight_is_refused_and_the_others_are_admitted() {
    let prev = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let rt = runtime();
    let _enter = rt.enter();
    let (a, b, c) = (workspace(ENABLED), workspace(ENABLED), workspace(ENABLED));
    let roots = vec![
        a.path().to_path_buf(),
        b.path().to_path_buf(),
        c.path().to_path_buf(),
    ];
    let gate = Gate::default();
    let mut d = RoleDispatcher::with_decide(
        spec("judge"),
        Duration::from_secs(300),
        gated_factory(vec![a.path().to_path_buf()], gate.clone()),
        queue_has_work(),
        None,
        production_decide(spec("judge")),
    );
    let in_progress = new_in_progress_guard();

    let first = d.dispatch_tick(roots.clone(), &in_progress);
    // B and C finish; A stays blocked.
    let finished: Vec<PathBuf> = (0..2)
        .map(|_| join_one(&rt, &mut d).unwrap().1.root)
        .collect();
    let second = d.dispatch_tick(roots.clone(), &in_progress);
    let a_still_running = d.in_flight_len();
    gate.open();
    while d.in_flight_len() > 0 {
        join_one(&rt, &mut d).unwrap();
    }
    match prev {
        Some(v) => std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v),
        None => std::env::remove_var(ROLE_RUNNER_ENABLE_ENV),
    }

    assert_eq!(first.spawned, roots, "first tick admits every root");
    assert!(!finished.contains(&roots[0]), "A is the one still blocked");
    assert_eq!(second.in_progress, vec![roots[0].clone()], "A refused as in progress");
    assert_eq!(second.spawned, roots[1..].to_vec(), "B and C admitted again");
    assert_eq!(a_still_running, 3, "no second instance of A was spawned");
}

// -- AC3: different roles in the same repository ----------------------------

/// AC3: judge and doctor on the same root are admitted at the same time — the
/// guard key is `(root, role)` and no role declares exclusivity.
#[test]
fn ac3_judge_and_doctor_run_in_the_same_repository_at_once() {
    let root = workspace(ENABLED);
    let in_progress = new_in_progress_guard();
    let config = read_role_runner_config(root.path());
    let judge = admit_root_tick(root.path(), "judge", "/loom:judge".into(), &in_progress, &config);
    let doctor =
        admit_root_tick(root.path(), "doctor", "/loom:doctor".into(), &in_progress, &config);
    assert!(matches!(judge, RootTickDecision::Admit { .. }));
    assert!(matches!(doctor, RootTickDecision::Admit { .. }));
    assert_eq!(active_run_count(&in_progress), 2);
}

// -- AC4: per-role budget ----------------------------------------------------

/// AC4: with `roleMaxConcurrent.judge = 2`, the third judge is refused with
/// `RoleBudgetReached` while curator on another root is still admitted; the
/// host ceiling still binds across every role together.
#[test]
fn ac4_role_budget_refuses_the_third_judge_but_not_another_role() {
    let set = new_in_progress_guard();
    let root = |n: u8| PathBuf::from(format!("/tmp/loom-9391-budget-{n}"));
    let _j1 = RoleRunGuard::admit_with_role_budget(set.clone(), root(1), "judge", 4, 2)
        .into_guard()
        .unwrap();
    let _j2 = RoleRunGuard::admit_with_role_budget(set.clone(), root(2), "judge", 4, 2)
        .into_guard()
        .unwrap();
    match RoleRunGuard::admit_with_role_budget(set.clone(), root(3), "judge", 4, 2) {
        RoleAdmission::RoleBudgetReached {
            role,
            active,
            budget,
        } => {
            assert_eq!((role, active, budget), ("judge", 2, 2));
        }
        other => panic!("expected RoleBudgetReached, got {other:?}"),
    }
    let _c = RoleRunGuard::admit_with_role_budget(set.clone(), root(4), "curator", 4, 2)
        .into_guard()
        .expect("curator has its own budget");
    let _d = RoleRunGuard::admit_with_role_budget(set.clone(), root(5), "doctor", 4, 2)
        .into_guard()
        .expect("fourth run fits the host ceiling of 4");
    // Host ceiling (4) now full: checked before the role budget.
    assert!(matches!(
        RoleRunGuard::admit_with_role_budget(set.clone(), root(6), "champion", 4, 2),
        RoleAdmission::CeilingReached {
            active: 4,
            ceiling: 4
        }
    ));
    // InProgress is checked first of all.
    assert!(matches!(
        RoleRunGuard::admit_with_role_budget(set, root(1), "judge", 4, 2),
        RoleAdmission::InProgress
    ));
}

/// AC4 through config: `roleMaxConcurrent.judge = 2` read from the root's own
/// config becomes a `Refused(RoleBudget)` decision for the third judge.
#[test]
fn ac4_role_budget_is_read_from_config_by_the_tick_decision() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"roleMaxConcurrent":{"judge":2}}}}"#;
    let roots: Vec<tempfile::TempDir> = (0..4).map(|_| workspace(cfg)).collect();
    let in_progress = new_in_progress_guard();
    let mut held = Vec::new();
    for t in &roots[..2] {
        let config = read_role_runner_config(t.path());
        held.push(admit_root_tick(t.path(), "judge", "/loom:judge".into(), &in_progress, &config));
    }
    let config = read_role_runner_config(roots[2].path());
    let third =
        admit_root_tick(roots[2].path(), "judge", "/loom:judge".into(), &in_progress, &config);
    assert!(matches!(
        third,
        RootTickDecision::Refused(LimitRefusal::RoleBudget {
            active: 2,
            budget: 2
        })
    ));
    let curator =
        admit_root_tick(roots[3].path(), "curator", "/loom:curator".into(), &in_progress, &config);
    assert!(matches!(curator, RootTickDecision::Admit { .. }));
}

/// AC4: idle-edge admissions (`plan_idle_runs`) count against the same
/// per-role budget as interval ticks.
#[test]
#[serial]
fn ac4_idle_edge_runs_count_against_the_role_budget() {
    let prev = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let root = workspace(r#"{"autonomous":{"roleRunner":{"roleMaxConcurrent":{"judge":1}}}}"#);
    let set = new_in_progress_guard();
    // An interval judge run elsewhere already holds the whole budget of 1.
    let _held = RoleRunGuard::admit_with_role_budget(
        set.clone(),
        PathBuf::from("/tmp/loom-9391-idle-other"),
        "judge",
        7,
        1,
    )
    .into_guard()
    .unwrap();
    let config = RoleRunnerConfig {
        enabled: Some(true),
        on_idle: Some(vec!["judge".to_string()]),
        ..RoleRunnerConfig::default()
    };
    let mut trigger = IdleTrigger::new();
    let now = Instant::now();
    let _ = plan_idle_runs(&mut trigger, &set, root.path(), &config, false, false, now);
    let fired = plan_idle_runs(&mut trigger, &set, root.path(), &config, true, false, now);
    match prev {
        Some(v) => std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v),
        None => std::env::remove_var(ROLE_RUNNER_ENABLE_ENV),
    }
    assert!(fired.is_empty(), "the idle judge must be refused by the judge budget of 1");
    assert_eq!(active_run_count(&set), 1);
}

// -- AC5: budget resolution --------------------------------------------------

#[test]
fn ac5_role_max_concurrent_parses_per_entry() {
    let block = serde_json::json!({
        "roleMaxConcurrent": {
            "judge": 2,
            " Doctor ": 4,
            "curator": 0,
            "champion": -1,
            "guide": "3",
            "auditor": 1.5,
            "not-a-role": 5,
            "": 9
        }
    });
    let parsed = parse_role_max_concurrent(&block);
    assert_eq!(
        parsed,
        BTreeMap::from([
            ("doctor".to_string(), 4),
            ("judge".to_string(), 2),
            ("not-a-role".to_string(), 5),
        ]),
        "valid entries kept (keys trimmed + lower-cased); zero, negative, non-integer and \
         blank-key entries dropped per entry"
    );
    assert!(parse_role_max_concurrent(&serde_json::json!({})).is_empty());
    assert!(parse_role_max_concurrent(&serde_json::json!({"roleMaxConcurrent": [1]})).is_empty());
}

#[test]
fn ac5_role_budget_resolution_defaults_and_clamps() {
    assert_eq!(default_role_max_concurrent(7), 3, "half the default ceiling");
    assert_eq!(default_role_max_concurrent(1), 1, "never zero");
    assert_eq!(default_role_max_concurrent(2), 1);

    let budgets = BTreeMap::from([
        ("judge".to_string(), 2),
        ("champion".to_string(), 50),
        ("not-a-role".to_string(), 1),
    ]);
    assert_eq!(resolve_role_max_concurrent(&budgets, "judge", 7), 2, "configured value");
    assert_eq!(
        resolve_role_max_concurrent(&budgets, "curator", 7),
        3,
        "absent ⇒ max(1, ceiling/2)"
    );
    assert_eq!(
        resolve_role_max_concurrent(&budgets, "champion", 7),
        7,
        "clamped to the ceiling"
    );
    assert_eq!(
        resolve_role_max_concurrent(&budgets, "doctor", 7),
        3,
        "an unknown role key does not affect known roles"
    );
    assert_eq!(resolve_role_max_concurrent(&BTreeMap::new(), "judge", 1), 1);
}

#[test]
fn ac5_role_max_concurrent_is_read_from_the_roots_config() {
    let root = workspace(r#"{"autonomous":{"roleRunner":{"roleMaxConcurrent":{"JUDGE":2}}}}"#);
    assert_eq!(
        read_role_max_concurrent(root.path()),
        BTreeMap::from([("judge".to_string(), 2)])
    );
    let absent = workspace(ENABLED);
    assert!(read_role_max_concurrent(absent.path()).is_empty());
}

// -- AC6: empty-queue gating ---------------------------------------------------

fn gate_outcome(role: &'static str, probe: QueueProbe) -> (RoleTickOutcome, usize) {
    let root = workspace(ENABLED);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runner = FixedRunner(RoleTickOutcome::Success, Arc::clone(&calls));
    let outcome =
        run_gated(&mut runner, &probe, root.path(), role, "/loom:x", Duration::from_secs(300));
    (outcome, calls.load(Ordering::SeqCst))
}

#[test]
fn ac6_judge_with_an_empty_queue_spawns_nothing() {
    let (outcome, invoked) = gate_outcome("judge", Arc::new(|_, _| Ok(false)));
    assert_eq!(outcome, RoleTickOutcome::QueueEmpty);
    assert_eq!(invoked, 0, "no agent spawned for an empty queue");
    assert!(!outcome.is_success());
    assert_eq!(
        crate::role_tick_telemetry::classify(&outcome).0,
        crate::telemetry::RoleTickResult::SkippedQueueEmpty
    );
    assert!(!crate::telemetry::RoleTickResult::SkippedQueueEmpty.spawned());
}

#[test]
fn ac6_a_non_empty_queue_dispatches_and_a_listing_error_fails_open() {
    let (outcome, invoked) = gate_outcome("doctor", Arc::new(|_, _| Ok(true)));
    assert_eq!((outcome, invoked), (RoleTickOutcome::Success, 1));
    let (outcome, invoked) = gate_outcome("judge", Arc::new(|_, _| Err("gh: 502".to_string())));
    assert_eq!((outcome, invoked), (RoleTickOutcome::Success, 1), "listing error ⇒ dispatch");
}

#[test]
fn ac6_ungated_roles_ignore_the_queue_probe() {
    assert_eq!(work_queue_labels("judge"), Some(&["loom:review-requested"][..]));
    assert_eq!(work_queue_labels("doctor"), Some(&["loom:changes-requested"][..]));
    for role in ["curator", "champion", "auditor", "hermit", "guide"] {
        assert_eq!(work_queue_labels(role), None, "{role} is ungated in Phase 1");
        let (outcome, invoked) = gate_outcome(role, Arc::new(|_, _| Ok(false)));
        assert_eq!((outcome, invoked), (RoleTickOutcome::Success, 1), "{role} dispatched");
    }
}

/// AC6: `QueueEmpty` never enters the #4349 fail/recover edge — a root that
/// was failing stays failing (no false "recovered"), and one that was not
/// does not become failing.
#[test]
#[serial(role_tick_ring)]
fn ac6_queue_empty_is_not_a_failure_and_leaves_the_fail_edge_alone() {
    let root = PathBuf::from("/tmp/loom-9391-queue-empty");
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::QueueEmpty,
            Duration::ZERO,
            true,
            false,
            false,
            false
        ),
        RootTickLogAction::QueueEmpty
    );
    assert!(!RootTickLogAction::QueueEmpty.is_failing());
    let mut failing = HashMap::from([(root.clone(), true)]);
    let (mut a, mut b, mut c) = (HashMap::new(), HashMap::new(), HashMap::new());
    log_outcome_for_root_deduped(
        "judge",
        &root,
        &RoleTickOutcome::QueueEmpty,
        Duration::ZERO,
        &mut failing,
        &mut a,
        &mut b,
        &mut c,
    );
    assert_eq!(failing.get(&root), Some(&true), "fail state untouched");
    assert!(a.is_empty() && b.is_empty() && c.is_empty());
}

// -- AC7: completion bookkeeping -----------------------------------------------

struct ScriptedRunner(Arc<Mutex<VecDeque<RoleTickOutcome>>>);

impl RoleInvocationRunner for ScriptedRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(RoleTickOutcome::Success)
    }
}

/// AC7: a reaped run still drives the #4349 fail/recover edges — WARN once on
/// the fail edge, DEBUG on the repeat, one INFO on recovery.
#[test]
#[serial(role_tick_ring)]
fn ac7_reaped_runs_log_the_fail_and_recover_edges_once() {
    let rt = runtime();
    let _enter = rt.enter();
    let root = workspace(ENABLED);
    let script = Arc::new(Mutex::new(VecDeque::from([
        RoleTickOutcome::Failure("boom".into()),
        RoleTickOutcome::Failure("boom".into()),
        RoleTickOutcome::Success,
    ])));
    let factory: RunnerFactory = {
        let script = Arc::clone(&script);
        Arc::new(move |_| Box::new(ScriptedRunner(Arc::clone(&script))))
    };
    let mut d = dispatcher("curator", factory, admit_only("curator"));
    let in_progress = new_in_progress_guard();
    let mut levels = Vec::new();
    for _ in 0..3 {
        d.dispatch_tick(vec![root.path().to_path_buf()], &in_progress);
        let joined = join_one(&rt, &mut d);
        let records = crate::test_log_capture::capture_logs(|| d.handle_joined(joined));
        let edge: Vec<log::Level> = records
            .iter()
            .filter(|(_, m)| m.contains("tick failed") || m.contains("recovered"))
            .map(|(l, _)| *l)
            .collect();
        levels.push(edge);
    }
    assert_eq!(levels[0], vec![log::Level::Warn], "fail edge warns once");
    assert_eq!(levels[1], vec![log::Level::Debug], "repeat failure is DEBUG");
    // A fake run is implausibly fast, so recovery takes the #4034 WARN form of
    // the recovered line; what matters here is that it is logged exactly once.
    assert_eq!(levels[2].len(), 1, "recovery logs exactly once");
    assert!(!d.is_failing(root.path()));
}

struct CountingObserver(AtomicUsize);

impl PoolExhaustedObserver for CountingObserver {
    fn note_pool_exhausted(&self, _root: &Path, _role: &str) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// AC7: a reaped pool-exhausted run still feeds the #6614/#7607 brake.
#[test]
#[serial(role_tick_ring)]
fn ac7_reaped_pool_exhausted_run_feeds_the_observer() {
    let rt = runtime();
    let _enter = rt.enter();
    let root = workspace(ENABLED);
    let observer = Arc::new(CountingObserver(AtomicUsize::new(0)));
    let factory: RunnerFactory = Arc::new(|_| {
        Box::new(FixedRunner(
            RoleTickOutcome::claude_pool_exhausted(3, chrono::Utc::now()),
            Arc::new(AtomicUsize::new(0)),
        ))
    });
    let mut d = RoleDispatcher::with_decide(
        spec("curator"),
        Duration::from_secs(300),
        factory,
        queue_has_work(),
        Some(observer.clone() as Arc<dyn PoolExhaustedObserver>),
        admit_only("curator"),
    );
    let in_progress = new_in_progress_guard();
    d.dispatch_tick(vec![root.path().to_path_buf()], &in_progress);
    let joined = join_one(&rt, &mut d);
    d.handle_joined(joined);
    assert_eq!(observer.0.load(Ordering::SeqCst), 1);
}

struct PanickingRunner;

impl RoleInvocationRunner for PanickingRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        panic!("runner exploded (#9391 test)");
    }
}

/// AC7: a panicking run logs the root it was running, releases its guard, and
/// the next tick admits that root again (#6201).
#[test]
fn ac7_a_panicking_run_logs_its_root_and_the_loop_keeps_ticking() {
    let rt = runtime();
    let _enter = rt.enter();
    let root = workspace(ENABLED);
    let mut d =
        dispatcher("curator", Arc::new(|_| Box::new(PanickingRunner)), admit_only("curator"));
    let in_progress = new_in_progress_guard();
    d.dispatch_tick(vec![root.path().to_path_buf()], &in_progress);
    let joined = join_one(&rt, &mut d);
    assert!(joined.as_ref().is_err_and(JoinError::is_panic));
    let records = crate::test_log_capture::capture_logs(|| d.handle_joined(joined));
    let root_str = root.path().display().to_string();
    assert!(
        records.iter().any(|(l, m)| *l == log::Level::Error
            && m.contains("panicked")
            && m.contains(&root_str)),
        "panic log must name the root: {records:?}"
    );
    assert_eq!(active_run_count(&in_progress), 0, "guard released by the unwind");
    let again = d.dispatch_tick(vec![root.path().to_path_buf()], &in_progress);
    assert_eq!(again.spawned.len(), 1, "the root is admitted again next tick");
    let _ = join_one(&rt, &mut d);
}

// -- AC8: refusal log volume ---------------------------------------------------

fn refusal_tick(config: &str, roots: usize) -> (TickReport, Vec<(log::Level, String)>) {
    let rt = runtime();
    let _enter = rt.enter();
    let dirs: Vec<tempfile::TempDir> = (0..roots).map(|_| workspace(config)).collect();
    let paths: Vec<PathBuf> = dirs.iter().map(|t| t.path().to_path_buf()).collect();
    let gate = Gate::default();
    let mut d =
        dispatcher("judge", gated_factory(paths.clone(), gate.clone()), admit_only("judge"));
    let in_progress = new_in_progress_guard();
    let mut report = TickReport::default();
    let records = crate::test_log_capture::capture_logs(|| {
        report = d.dispatch_tick(paths.clone(), &in_progress);
    });
    gate.open();
    while d.in_flight_len() > 0 {
        let _ = join_one(&rt, &mut d);
    }
    (report, records)
}

fn refusal_lines(records: &[(log::Level, String)]) -> Vec<&(log::Level, String)> {
    records
        .iter()
        .filter(|(l, m)| {
            *l >= log::Level::Warn
                && (m.contains("not admitted") || m.contains("stopped admitting"))
        })
        .collect()
}

#[test]
fn ac8_a_ceiling_refusal_logs_one_line_per_tick_not_one_per_root() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"maxConcurrent":1}}}"#;
    let (report, records) = refusal_tick(cfg, 5);
    assert_eq!(report.spawned.len(), 1);
    assert_eq!(
        report.refusal,
        Some(LimitRefusal::Ceiling {
            active: 1,
            ceiling: 1
        })
    );
    assert_eq!(report.deferred, 4);
    let lines = refusal_lines(&records);
    assert_eq!(lines.len(), 1, "one aggregated line: {records:?}");
    assert!(lines[0].1.contains("4 root(s) deferred"), "{}", lines[0].1);
}

#[test]
fn ac8_a_role_budget_refusal_logs_one_line_per_tick() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"roleMaxConcurrent":{"judge":2}}}}"#;
    let (report, records) = refusal_tick(cfg, 5);
    assert_eq!(report.spawned.len(), 2);
    assert_eq!(
        report.refusal,
        Some(LimitRefusal::RoleBudget {
            active: 2,
            budget: 2
        })
    );
    assert_eq!(report.deferred, 3);
    let lines = refusal_lines(&records);
    assert_eq!(lines.len(), 1, "one aggregated line: {records:?}");
    assert!(lines[0].1.contains("roleMaxConcurrent"), "{}", lines[0].1);
}
