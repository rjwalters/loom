//! Tests for per-repository doctor lanes (#10632): the width formula, its
//! per-repository input, lane admission against the host limits, distinct PR
//! assignment, and the dispatcher wiring that turns a lane into
//! `/loom:doctor <PR>`.
//!
//! Each test uses its own leaked [`demand::DemandLedger`] and an injected lane
//! probe, so none reads the global ledger or the forge. Roots are unique
//! tempdirs (or unique paths), so the host-wide assignment table never mixes
//! two tests' entries.

use super::*;
use demand::{DebtAxis, DemandConfig, DemandLedger, HostDebt};
use std::sync::atomic::AtomicUsize;
use std::sync::Condvar;

const ENABLED: &str = r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#;

fn workspace(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(tmp.path().join(".loom").join("config.json"), config).unwrap();
    tmp
}

fn ledger() -> &'static DemandLedger {
    Box::leak(Box::new(DemandLedger::default()))
}

fn admit(
    root: &Path,
    role: &'static str,
    in_progress: &InProgressGuard,
    ledger: &DemandLedger,
) -> RootTickDecision {
    let config = read_role_runner_config(root);
    admit_root_tick_with(root, role, format!("/loom:{role}"), in_progress, &config, ledger)
}

fn lane_of(decision: &RootTickDecision) -> Option<(usize, bool)> {
    match decision {
        RootTickDecision::Admit { guard, assign, .. } => Some((guard.lane(), *assign)),
        _ => None,
    }
}

fn probe(rows: Vec<u64>, stale: &'static [u64]) -> LaneProbe {
    LaneProbe {
        queue: Arc::new(move |_| Ok(rows.clone())),
        verdict: Arc::new(move |_, pr| !stale.contains(&pr)),
    }
}

// -- width -------------------------------------------------------------------

#[test]
fn doctor_width_follows_its_own_repository_debt_clamped_to_max_per_repo() {
    let cfg = DemandConfig::default(); // perRun 3, doctorMaxPerRepo 3
    let changes = |n: usize| {
        let ledger = DemandLedger::default();
        let root = PathBuf::from("/tmp/loom-10632-width");
        ledger.record(&root, DebtAxis::Changes, n);
        ledger.repo_debt(&root, cfg.stale())
    };
    assert_eq!(demand::repo_lanes("doctor", &changes(1), &cfg), 1);
    assert_eq!(demand::repo_lanes("doctor", &changes(3), &cfg), 1);
    assert_eq!(demand::repo_lanes("doctor", &changes(4), &cfg), 2);
    assert_eq!(demand::repo_lanes("doctor", &changes(60), &cfg), 3, "clamped");
    assert_eq!(demand::repo_lanes("doctor", &changes(0), &cfg), 1, "drained ⇒ classic");
    assert_eq!(
        demand::repo_lanes("doctor", &HostDebt::default(), &cfg),
        1,
        "unobserved ⇒ classic"
    );
    for role in ["judge", "champion", "curator"] {
        assert_eq!(demand::repo_lanes(role, &changes(60), &cfg), 1, "{role} keeps one");
    }
    let off = DemandConfig {
        doctor_max_per_repo: 1,
        ..cfg
    };
    assert_eq!(demand::repo_lanes("doctor", &changes(60), &off), 1, "1 disables lanes");
}

#[test]
fn repo_debt_reads_one_repository_not_the_host_total() {
    let ledger = DemandLedger::default();
    let (hot, cold) = (PathBuf::from("/tmp/loom-10632-hot"), PathBuf::from("/tmp/loom-10632-cold"));
    ledger.record(&hot, DebtAxis::Changes, 60);
    ledger.record(&cold, DebtAxis::Changes, 2);
    let stale = Duration::from_secs(1800);
    let cfg = DemandConfig::default();
    assert_eq!(ledger.repo_debt(&hot, stale).axis_width(DebtAxis::Changes), Some(60));
    assert_eq!(ledger.repo_debt(&cold, stale).axis_width(DebtAxis::Changes), Some(2));
    assert_eq!(ledger.host_debt(stale).axis_width(DebtAxis::Changes), Some(62));
    assert_eq!(demand::repo_lanes("doctor", &ledger.repo_debt(&cold, stale), &cfg), 1);
    // A stale per-repository entry is unobserved: no extra lanes on old data.
    let old = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .expect("monotonic clock is past boot + 10s");
    ledger.record_at(&hot, DebtAxis::Changes, 60, old);
    let fresh_only = Duration::from_secs(5);
    assert_eq!(
        ledger
            .repo_debt(&hot, fresh_only)
            .axis_width(DebtAxis::Changes),
        None
    );
}

#[test]
fn doctor_max_per_repo_parses_and_is_bounded() {
    let parse = |v: serde_json::Value| {
        demand::parse_demand_config(
            &serde_json::json!({ "demandWidth": { "doctorMaxPerRepo": v } }),
        )
        .doctor_max_per_repo
    };
    assert_eq!(parse(serde_json::json!(5)), 5);
    assert_eq!(parse(serde_json::json!(1)), 1);
    assert_eq!(parse(serde_json::json!(0)), 3, "zero drops to the default");
    assert_eq!(parse(serde_json::json!("4")), 3, "non-integer drops to the default");
    assert_eq!(
        parse(serde_json::json!(1000)),
        demand::DOCTOR_MAX_PER_REPO_LIMIT,
        "a huge value is clamped"
    );
}

// -- admission ---------------------------------------------------------------

#[test]
fn a_hot_repository_admits_one_doctor_lane_per_decision_up_to_its_width() {
    let ws = workspace(ENABLED);
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 60);
    let in_progress = new_in_progress_guard();
    let decisions: Vec<_> = (0..4)
        .map(|_| admit(ws.path(), "doctor", &in_progress, ledger))
        .collect();
    let lanes: Vec<_> = decisions.iter().map(lane_of).collect();
    assert_eq!(
        lanes,
        vec![Some((0, true)), Some((1, true)), Some((2, true)), None],
        "{decisions:?}"
    );
    assert!(matches!(decisions[3], RootTickDecision::InProgress), "every lane in flight");
    // A finished lane frees exactly its own slot.
    let mut decisions = decisions;
    decisions.remove(1);
    let again = admit(ws.path(), "doctor", &in_progress, ledger);
    assert_eq!(lane_of(&again), Some((1, true)));
}

#[test]
fn a_cold_repository_and_other_roles_keep_one_instance() {
    let ws = workspace(ENABLED);
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 2);
    ledger.record(ws.path(), DebtAxis::Review, 60);
    let in_progress = new_in_progress_guard();
    let first = admit(ws.path(), "doctor", &in_progress, ledger);
    assert_eq!(lane_of(&first), Some((0, false)), "classic, unassigned run");
    assert!(matches!(
        admit(ws.path(), "doctor", &in_progress, ledger),
        RootTickDecision::InProgress
    ));
    let judge = admit(ws.path(), "judge", &in_progress, ledger);
    assert_eq!(lane_of(&judge), Some((0, false)));
    assert!(matches!(
        admit(ws.path(), "judge", &in_progress, ledger),
        RootTickDecision::InProgress
    ));
}

#[test]
fn lanes_still_count_against_the_host_ceiling_and_doctor_budget() {
    // `roleMaxConcurrent` lifts doctor's budget to the ceiling, so the
    // ceiling is the limit that binds.
    let ceiling_two = workspace(
        r#"{"autonomous":{"roleRunner":{"enabled":true,"maxConcurrent":2,"roleMaxConcurrent":{"doctor":2}}}}"#,
    );
    let ledger = ledger();
    ledger.record(ceiling_two.path(), DebtAxis::Changes, 60);
    let in_progress = new_in_progress_guard();
    let held: Vec<_> = (0..2)
        .map(|_| admit(ceiling_two.path(), "doctor", &in_progress, ledger))
        .collect();
    assert!(held.iter().all(|d| lane_of(d).is_some()), "{held:?}");
    assert!(matches!(
        admit(ceiling_two.path(), "doctor", &in_progress, ledger),
        RootTickDecision::Refused(LimitRefusal::Ceiling {
            active: 2,
            ceiling: 2
        })
    ));

    // Budget: doctor's host budget is 3 at the default ceiling, so a hot repo
    // holding three lanes leaves another repository's doctor refused.
    let (hot, other) = (workspace(ENABLED), workspace(ENABLED));
    let ledger = self::ledger();
    ledger.record(hot.path(), DebtAxis::Changes, 60);
    ledger.record(other.path(), DebtAxis::Changes, 1);
    let in_progress = new_in_progress_guard();
    let _lanes: Vec<_> = (0..3)
        .map(|_| admit(hot.path(), "doctor", &in_progress, ledger))
        .collect();
    let refused = admit(other.path(), "doctor", &in_progress, ledger);
    assert!(
        matches!(refused, RootTickDecision::Refused(LimitRefusal::RoleBudget { active: 3, .. })),
        "{refused:?}"
    );
}

#[test]
fn doctor_max_per_repo_one_is_the_classic_admission() {
    let ws = workspace(
        r#"{"autonomous":{"roleRunner":{"enabled":true,"demandWidth":{"doctorMaxPerRepo":1}}}}"#,
    );
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 60);
    let in_progress = new_in_progress_guard();
    let first = admit(ws.path(), "doctor", &in_progress, ledger);
    assert_eq!(lane_of(&first), Some((0, false)));
    assert!(matches!(
        admit(ws.path(), "doctor", &in_progress, ledger),
        RootTickDecision::InProgress
    ));
}

// -- assignment --------------------------------------------------------------

#[test]
fn concurrent_lanes_are_assigned_distinct_prs_and_a_finished_lane_frees_its_pr() {
    let root = PathBuf::from("/tmp/loom-10632-assign-distinct");
    let p = probe(vec![11, 12, 13], &[]);
    let LaneTarget::Pr(a) = assign(&p, &root, 0) else {
        panic!("lane 0 gets the queue head")
    };
    let LaneTarget::Pr(b) = assign(&p, &root, 1) else {
        panic!("lane 1 gets the next row")
    };
    assert_eq!((a.pr(), b.pr()), (11, 12));
    // Another repository's lanes do not see this one's holds.
    let LaneTarget::Pr(elsewhere) = assign(&p, Path::new("/tmp/loom-10632-assign-other"), 1) else {
        panic!("a different root starts at its own head")
    };
    assert_eq!(elsewhere.pr(), 11);
    drop(a);
    let LaneTarget::Pr(c) = assign(&p, &root, 2) else {
        panic!("the freed head is assignable again")
    };
    assert_eq!(c.pr(), 11);
}

#[test]
fn stale_verdicts_are_skipped_and_the_guard_runs_on_a_bounded_number_of_rows() {
    let root = PathBuf::from("/tmp/loom-10632-assign-stale");
    let LaneTarget::Pr(hold) = assign(&probe(vec![21, 22, 23], &[21, 22]), &root, 1) else {
        panic!("the first fresh row is taken")
    };
    assert_eq!(hold.pr(), 23);
    drop(hold);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let all_stale = LaneProbe {
        queue: Arc::new(|_| Ok((100..120).collect())),
        verdict: Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            false
        }),
    };
    assert!(matches!(assign(&all_stale, &root, 1), LaneTarget::Nothing));
    assert_eq!(calls.load(Ordering::SeqCst), VERDICT_ATTEMPTS);
    assert!(LaneAssignment::reserve(&root, 100).is_some(), "a rejected row is not left held");
}

#[test]
fn an_unreadable_queue_runs_lane_zero_classic_and_stands_extra_lanes_down() {
    let root = PathBuf::from("/tmp/loom-10632-assign-error");
    let none = LaneProbe::none();
    assert!(matches!(assign(&none, &root, 0), LaneTarget::Unassigned));
    assert!(matches!(assign(&none, &root, 1), LaneTarget::Nothing));
    assert!(matches!(assign(&probe(vec![], &[]), &root, 0), LaneTarget::Nothing));
}

// -- dispatcher --------------------------------------------------------------

/// Runs record their prompt and block until released, so lanes stay in flight
/// across ticks.
#[derive(Clone, Default)]
struct Held {
    prompts: Arc<Mutex<Vec<String>>>,
    open: Arc<(Mutex<bool>, Condvar)>,
}

impl Held {
    fn release(&self) {
        let (lock, cvar) = &*self.open;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    fn factory(&self) -> RunnerFactory {
        let held = self.clone();
        Arc::new(move |_| Box::new(HeldRunner(held.clone())))
    }
}

struct HeldRunner(Held);

impl RoleInvocationRunner for HeldRunner {
    fn invoke(&mut self, _role: &str, prompt: &str) -> RoleTickOutcome {
        self.0.prompts.lock().unwrap().push(prompt.to_string());
        let (lock, cvar) = &*self.0.open;
        let open = lock.lock().unwrap();
        let (_open, timeout) = cvar
            .wait_timeout_while(open, Duration::from_secs(30), |o| !*o)
            .unwrap();
        assert!(!timeout.timed_out(), "run was never released");
        RoleTickOutcome::Success
    }
}

fn doctor_dispatcher(
    held: &Held,
    ledger: &'static DemandLedger,
    lane_probe: LaneProbe,
) -> RoleDispatcher {
    let spec = *DEFAULT_ROLES.iter().find(|s| s.name == "doctor").unwrap();
    RoleDispatcher::with_decide(
        spec,
        Duration::from_secs(300),
        held.factory(),
        Arc::new(|_, _| Ok(true)),
        None,
        Box::new(move |root, in_progress, _| admit(root, "doctor", in_progress, ledger)),
    )
    .with_demand(ledger, demand::no_merge_probe())
    .with_lane_probe(lane_probe)
}

fn run_to_completion(rt: &tokio::runtime::Runtime, d: &mut RoleDispatcher) -> Vec<RoleTickOutcome> {
    let mut outcomes = Vec::new();
    while d.in_flight_len() > 0 {
        let (_, run) = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(30), d.join_next()).await })
            .expect("a run finished within 30s")
            .expect("a run was in flight")
            .expect("the run did not panic");
        outcomes.push(run.outcome);
    }
    outcomes
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Three ticks on one hot repository put three doctors in flight at once,
/// each dispatched as `/loom:doctor <PR>` on a different PR.
#[test]
fn a_hot_repository_runs_concurrent_doctors_on_different_assigned_prs() {
    let rt = runtime();
    let _enter = rt.enter();
    let ws = workspace(ENABLED);
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 60);
    let held = Held::default();
    let mut doctor = doctor_dispatcher(&held, ledger, probe(vec![31, 32, 33, 34], &[]));
    let in_progress = new_in_progress_guard();
    for _ in 0..4 {
        doctor.dispatch_tick(vec![ws.path().to_path_buf()], &in_progress);
    }
    assert_eq!(doctor.in_flight_len(), 3, "width 3: the fourth tick finds every lane busy");
    let deadline = Instant::now() + Duration::from_secs(30);
    while held.prompts.lock().unwrap().len() < 3 {
        assert!(Instant::now() < deadline, "all three lanes reached invoke");
        std::thread::sleep(Duration::from_millis(10));
    }
    held.release();
    run_to_completion(&rt, &mut doctor);
    let mut prompts = held.prompts.lock().unwrap().clone();
    prompts.sort();
    assert_eq!(prompts, ["/loom:doctor 31", "/loom:doctor 32", "/loom:doctor 33"]);
}

/// With fewer assignable PRs than lanes, the extra lane ends `QueueEmpty`
/// without spawning an agent.
#[test]
fn a_lane_with_nothing_to_assign_spends_no_agent() {
    let rt = runtime();
    let _enter = rt.enter();
    let ws = workspace(ENABLED);
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 60);
    let held = Held::default();
    let mut doctor = doctor_dispatcher(&held, ledger, probe(vec![41], &[]));
    let in_progress = new_in_progress_guard();
    doctor.dispatch_tick(vec![ws.path().to_path_buf()], &in_progress);
    let deadline = Instant::now() + Duration::from_secs(30);
    while held.prompts.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "lane 0 reached invoke");
        std::thread::sleep(Duration::from_millis(10));
    }
    doctor.dispatch_tick(vec![ws.path().to_path_buf()], &in_progress);
    let outcomes = {
        // Lane 1 finishes on its own (QueueEmpty) while lane 0 is still held.
        let (_, run) = rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(30), doctor.join_next()).await
            })
            .expect("lane 1 finished")
            .expect("in flight")
            .expect("no panic");
        run.outcome
    };
    assert!(matches!(outcomes, RoleTickOutcome::QueueEmpty), "{outcomes:?}");
    held.release();
    run_to_completion(&rt, &mut doctor);
    assert_eq!(*held.prompts.lock().unwrap(), ["/loom:doctor 41"]);
}

/// A cold repository's doctor is the classic, unassigned `/loom:doctor`.
#[test]
fn a_cold_repository_dispatches_the_classic_prompt() {
    let rt = runtime();
    let _enter = rt.enter();
    let ws = workspace(ENABLED);
    let ledger = ledger();
    ledger.record(ws.path(), DebtAxis::Changes, 2);
    let held = Held::default();
    held.release();
    let mut doctor = doctor_dispatcher(&held, ledger, probe(vec![51], &[]));
    doctor.dispatch_tick(vec![ws.path().to_path_buf()], &new_in_progress_guard());
    run_to_completion(&rt, &mut doctor);
    assert_eq!(*held.prompts.lock().unwrap(), ["/loom:doctor"]);
}
