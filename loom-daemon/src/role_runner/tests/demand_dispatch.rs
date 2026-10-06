//! Dispatcher-level tests for demand-weighted admission (#9392): width and
//! reservation through `admit_root_tick_with`, the ledger feeds, the
//! `enabled: false` parity, and round-robin fairness under a reservation.
//!
//! Each test injects its own leaked [`demand::DemandLedger`], stub queue and
//! merge probes, so none reads the global ledger or the network.

use super::*;
use demand::{DebtAxis, DemandLedger, DemandProbe};
use std::sync::atomic::AtomicUsize;
use std::sync::Condvar;

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

fn workspaces(config: &str, n: usize) -> (Vec<tempfile::TempDir>, Vec<PathBuf>) {
    let dirs: Vec<tempfile::TempDir> = (0..n).map(|_| workspace(config)).collect();
    let paths = dirs.iter().map(|t| t.path().to_path_buf()).collect();
    (dirs, paths)
}

const ENABLED: &str = r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn ledger() -> &'static DemandLedger {
    Box::leak(Box::new(DemandLedger::default()))
}

fn admit_with(role: &'static str, ledger: &'static DemandLedger) -> DecideFn {
    Box::new(move |root, in_progress, _| {
        admit_root_tick_with(
            root,
            role,
            format!("/loom:{role}"),
            in_progress,
            &read_role_runner_config(root),
            ledger,
        )
    })
}

/// A merge probe returning `value`, and how many times it was called.
fn counting(value: Result<usize, String>) -> (DemandProbe, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let f = Arc::new(move |_: &Path| {
        c.fetch_add(1, Ordering::SeqCst);
        value.clone()
    });
    (f, calls)
}

struct CountingRunner(Arc<AtomicUsize>);

impl RoleInvocationRunner for CountingRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        RoleTickOutcome::Success
    }
}

fn counting_factory() -> (RunnerFactory, Arc<AtomicUsize>) {
    let invoked = Arc::new(AtomicUsize::new(0));
    let i = Arc::clone(&invoked);
    let factory: RunnerFactory = Arc::new(move |_| Box::new(CountingRunner(Arc::clone(&i))));
    (factory, invoked)
}

fn drain(rt: &tokio::runtime::Runtime, d: &mut RoleDispatcher) {
    while d.in_flight_len() > 0 {
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), d.join_next()).await })
            .expect("a run finished within 30s")
            .expect("a run was in flight")
            .expect("the run did not panic");
    }
}

fn record_all(ledger: &DemandLedger, roots: &[PathBuf], axis: DebtAxis, each: usize) {
    for root in roots {
        ledger.record(root, axis, each);
    }
}

// -- width -------------------------------------------------------------------

/// Judge is refused at its width (1, for 3 queued PRs) while its Phase 1
/// budget (3) still has room, and the refusal reports the width. Champion's
/// budget is unchanged by any merge debt.
#[test]
fn judge_is_refused_at_width_while_the_phase1_budget_has_room() {
    let (_dirs, roots) = workspaces(ENABLED, 4);
    let ledger = ledger();
    ledger.record(&roots[0], DebtAxis::Review, 3);
    ledger.record(&roots[0], DebtAxis::Merge, 1000);
    let in_progress = new_in_progress_guard();
    let config = read_role_runner_config(&roots[0]);
    let admit = |root: &PathBuf, role: &'static str| {
        admit_root_tick_with(root, role, "/loom:x".into(), &in_progress, &config, ledger)
    };
    let first = admit(&roots[0], "judge");
    assert!(matches!(first, RootTickDecision::Admit { .. }));
    let refused = admit(&roots[1], "judge");
    let RootTickDecision::Refused(refusal) = refused else {
        panic!("expected a width refusal, got {refused:?}");
    };
    assert_eq!(
        refusal,
        LimitRefusal::DemandWidth {
            active: 1,
            width: 1,
            budget: 3,
            debt: 3
        }
    );
    let line = refusal_summary_line("judge", refusal, 2);
    assert!(line.contains("demand width of 1 for 3 queued PR(s)"), "{line}");
    // Champion: Phase 1 budget 3, whatever the merge debt.
    let champions: Vec<_> = roots.iter().map(|r| admit(r, "champion")).collect();
    let admitted = champions
        .iter()
        .filter(|d| matches!(d, RootTickDecision::Admit { .. }))
        .count();
    assert_eq!(admitted, 3, "{champions:?}");
    assert!(matches!(
        champions[3],
        RootTickDecision::Refused(LimitRefusal::RoleBudget {
            active: 3,
            budget: 3
        })
    ));
}

/// `demandWidth.enabled: false` is Phase 1: the ledger is never read, judge
/// runs to its Phase 1 budget however little debt the ledger shows, and an
/// admitted champion run makes no `loom:pr` listing.
///
/// Runs hold their slots until each tick has returned (gated runners): an
/// immediately-returning runner can finish mid-walk on the 2-worker runtime
/// and free a slot, letting the 4th root in over the budget of 3 (#9737).
/// The body repeats so a regression of that race shows up here, not in CI.
#[test]
fn disabled_is_phase1_admission_with_no_ledger_read_or_listing() {
    for _ in 0..20 {
        disabled_is_phase1_admission_once();
    }
}

fn disabled_is_phase1_admission_once() {
    let rt = runtime();
    let _enter = rt.enter();
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"demandWidth":{"enabled":false}}}}"#;
    let (_dirs, roots) = workspaces(cfg, 4);
    let ledger = ledger();
    record_all(ledger, &roots, DebtAxis::Review, 1);
    record_all(ledger, &roots, DebtAxis::Merge, 50);
    let reads_before = ledger.reads();
    let in_progress = new_in_progress_guard();
    let gate = Rounds::default();
    let mut judge = RoleDispatcher::with_decide(
        spec("judge"),
        Duration::from_secs(300),
        gate.factory(),
        Arc::new(|_, _| Ok(true)),
        None,
        admit_with("judge", ledger),
    )
    .with_demand(ledger, demand::no_merge_probe());
    let tick = judge.dispatch_tick(roots.clone(), &in_progress);
    // The tick has returned, so its count and refusal are fixed: release
    // exactly its runs (tickets 1..=spawned) before asserting, so a failure
    // does not leave them blocked on the gate, while champion's runs below
    // (later tickets) stay gated until champion's tick returns.
    gate.release(tick.spawned.len());
    assert_eq!(tick.spawned.len(), 3, "Phase 1 budget, not width 1");
    assert_eq!(
        tick.refusal,
        Some(LimitRefusal::RoleBudget {
            active: 3,
            budget: 3
        })
    );
    drain(&rt, &mut judge);

    let (probe, calls) = counting(Ok(7));
    let mut champion = RoleDispatcher::with_decide(
        spec("champion"),
        Duration::from_secs(300),
        gate.factory(),
        Arc::new(|_, _| Ok(true)),
        None,
        admit_with("champion", ledger),
    )
    .with_demand(ledger, probe);
    let tick = champion.dispatch_tick(roots, &in_progress);
    gate.release(usize::MAX);
    assert_eq!(tick.spawned.len(), 3, "Phase 1 budget");
    assert_eq!(
        tick.refusal,
        Some(LimitRefusal::RoleBudget {
            active: 3,
            budget: 3
        })
    );
    drain(&rt, &mut champion);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no champion listing");
    assert_eq!(ledger.reads(), reads_before, "no ledger read");
}

// -- ledger feeds --------------------------------------------------------------

/// Judge and doctor make exactly the Phase 1 queue-gate listing — no merge
/// listing and no second queue call.
#[test]
fn judge_and_doctor_make_no_additional_listing() {
    let rt = runtime();
    let _enter = rt.enter();
    let (_dirs, roots) = workspaces(ENABLED, 1);
    for role in ["judge", "doctor"] {
        let ledger = ledger();
        let queue_calls = Arc::new(AtomicUsize::new(0));
        let q = Arc::clone(&queue_calls);
        let queue: QueueProbe = Arc::new(move |_, _| {
            q.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        });
        let (merge, merge_calls) = counting(Ok(3));
        let (factory, invoked) = counting_factory();
        let mut d = RoleDispatcher::with_decide(
            spec(role),
            Duration::from_secs(300),
            factory,
            queue,
            None,
            admit_with(role, ledger),
        )
        .with_demand(ledger, merge);
        assert_eq!(
            d.dispatch_tick(roots.clone(), &in_progress_guard())
                .spawned
                .len(),
            1
        );
        drain(&rt, &mut d);
        assert_eq!(invoked.load(Ordering::SeqCst), 1, "{role} ran");
        assert_eq!(queue_calls.load(Ordering::SeqCst), 1, "{role}: one queue listing");
        assert_eq!(merge_calls.load(Ordering::SeqCst), 0, "{role}: no merge listing");
    }
}

fn in_progress_guard() -> InProgressGuard {
    new_in_progress_guard()
}

/// Each admitted champion run records `loom:pr` exactly once, and an empty
/// listing still invokes the runner: the count never gates champion.
#[test]
fn champion_records_merge_debt_once_per_run_and_is_never_gated() {
    let rt = runtime();
    let _enter = rt.enter();
    let (_dirs, roots) = workspaces(ENABLED, 2);
    let ledger = ledger();
    let (merge, merge_calls) = counting(Ok(0));
    let (factory, invoked) = counting_factory();
    let mut d = RoleDispatcher::with_decide(
        spec("champion"),
        Duration::from_secs(300),
        factory,
        Arc::new(|_, _| Ok(false)),
        None,
        admit_with("champion", ledger),
    )
    .with_demand(ledger, merge);
    let in_progress = in_progress_guard();
    for _ in 0..2 {
        assert_eq!(d.dispatch_tick(roots.clone(), &in_progress).spawned.len(), 2);
        drain(&rt, &mut d);
    }
    assert_eq!(invoked.load(Ordering::SeqCst), 4, "an empty loom:pr never gates");
    assert_eq!(merge_calls.load(Ordering::SeqCst), 4, "one listing per admitted run");
    assert_eq!(
        ledger.host_debt(Duration::from_secs(60)).merge,
        Some(demand::AxisDebt::default()),
        "the empty listing is recorded as an observed 0"
    );
}

// -- reservation through the walk ----------------------------------------------

/// Runners that block until released: each invocation takes a ticket and
/// waits until `release(n)` has covered it, so a round's runs all hold their
/// slots until the round ends.
#[derive(Clone, Default)]
struct Rounds {
    issued: Arc<AtomicUsize>,
    released: Arc<(Mutex<usize>, Condvar)>,
}

impl Rounds {
    fn release(&self, upto: usize) {
        let (lock, cvar) = &*self.released;
        *lock.lock().unwrap() = upto;
        cvar.notify_all();
    }

    fn factory(&self) -> RunnerFactory {
        let rounds = self.clone();
        Arc::new(move |_| Box::new(RoundRunner(rounds.clone())))
    }
}

struct RoundRunner(Rounds);

impl RoleInvocationRunner for RoundRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        let ticket = self.0.issued.fetch_add(1, Ordering::SeqCst) + 1;
        let (lock, cvar) = &*self.0.released;
        let released = lock.lock().unwrap();
        let (_released, timeout) = cvar
            .wait_timeout_while(released, Duration::from_secs(30), |r| *r < ticket)
            .unwrap();
        assert!(!timeout.timed_out(), "run {ticket} was never released");
        RoleTickOutcome::Success
    }
}

/// Per round: curator, judge, then champion tick while every earlier run of
/// the round still holds its slot; then all runs finish. Returns, per role,
/// the runs each root got in each round.
fn run_rounds(config: &str, roots: usize, rounds: usize) -> Vec<[Vec<usize>; 3]> {
    let rt = runtime();
    let _enter = rt.enter();
    let (_dirs, paths) = workspaces(config, roots);
    let ledger = ledger();
    record_all(ledger, &paths, DebtAxis::Merge, 3);
    record_all(ledger, &paths, DebtAxis::Review, 3);
    let gate = Rounds::default();
    let mut dispatchers: Vec<RoleDispatcher> = ["curator", "judge", "champion"]
        .into_iter()
        .map(|role| {
            RoleDispatcher::with_decide(
                spec(role),
                Duration::from_secs(300),
                gate.factory(),
                Arc::new(|_, _| Ok(true)),
                None,
                admit_with(role, ledger),
            )
            .with_demand(ledger, demand::no_merge_probe())
        })
        .collect();
    let in_progress = new_in_progress_guard();
    let mut spawned_total = 0;
    let mut history = Vec::new();
    for _ in 0..rounds {
        let mut round: [Vec<usize>; 3] = Default::default();
        for (i, d) in dispatchers.iter_mut().enumerate() {
            round[i] = vec![0; roots];
            for root in d.dispatch_tick(paths.clone(), &in_progress).spawned {
                round[i][paths.iter().position(|p| *p == root).unwrap()] += 1;
                spawned_total += 1;
            }
        }
        gate.release(spawned_total);
        for d in &mut dispatchers {
            drain(&rt, d);
        }
        history.push(round);
    }
    history
}

fn first_round_reaching_every_root(history: &[[Vec<usize>; 3]], role: usize) -> Option<usize> {
    let roots = history.first()?[role].len();
    let mut seen = vec![false; roots];
    for (n, round) in history.iter().enumerate() {
        for (r, runs) in round[role].iter().enumerate() {
            seen[r] |= *runs > 0;
        }
        if seen.iter().all(|s| *s) {
            return Some(n + 1);
        }
    }
    None
}

/// With the reservation active, every repo with debt is still reached within
/// a bounded number of ticks by every role, and the rotation stays even:
/// ceiling 4 (budgets 2), merge and review debt on all 5 roots, curator
/// ticking first each round. Curator keeps its `nonPrFloor` slot, judge is
/// held back only by champion's unfilled want, and champion takes the rest —
/// so per round it is curator 1, judge 1, champion 2, and champion reaches all
/// 5 roots in `ceil(5 / 2) = 3` rounds, judge and curator in 5.
///
/// The control, `reserve: false`, is Phase 1's order-dependent allocation:
/// curator and judge take the ceiling first and champion never runs.
#[test]
fn fairness_every_repo_with_debt_is_reached_within_bounded_ticks_under_reservation() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"maxConcurrent":4}}}"#;
    let history = run_rounds(cfg, 5, 5);
    for round in &history {
        let per_role: Vec<usize> = round.iter().map(|r| r.iter().sum()).collect();
        assert_eq!(per_role, vec![1, 1, 2], "curator, judge, champion per round: {history:?}");
    }
    assert_eq!(first_round_reaching_every_root(&history, 2), Some(3), "champion");
    assert_eq!(first_round_reaching_every_root(&history, 1), Some(5), "judge");
    assert_eq!(first_round_reaching_every_root(&history, 0), Some(5), "curator");
    for role in 0..3 {
        let totals: Vec<usize> = (0..5)
            .map(|r| history.iter().map(|h| h[role][r]).sum())
            .collect();
        let (min, max) = (totals.iter().min().unwrap(), totals.iter().max().unwrap());
        assert!(max - min <= 1, "role {role} rotates evenly: {totals:?}");
    }

    let control = r#"{"autonomous":{"roleRunner":{"enabled":true,"maxConcurrent":4,
        "demandWidth":{"reserve":false}}}}"#;
    let phase1 = run_rounds(control, 5, 3);
    for round in &phase1 {
        let per_role: Vec<usize> = round.iter().map(|r| r.iter().sum()).collect();
        assert_eq!(per_role, vec![2, 2, 0], "without reservation champion starves");
    }
}

/// A reservation refusal is one summary line naming the reserved count and
/// the PR roles it is held for.
#[test]
fn a_reservation_refusal_logs_one_summary_line_naming_the_roles() {
    let rt = runtime();
    let _enter = rt.enter();
    // Curator's own budget raised to the ceiling, so the reservation binds.
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"maxConcurrent":4,
        "roleMaxConcurrent":{"curator":4}}}}"#;
    let (_dirs, roots) = workspaces(cfg, 5);
    let ledger = ledger();
    record_all(ledger, &roots, DebtAxis::Merge, 3);
    let gate = Rounds::default();
    let mut curator = RoleDispatcher::with_decide(
        spec("curator"),
        Duration::from_secs(300),
        gate.factory(),
        Arc::new(|_, _| Ok(true)),
        None,
        admit_with("curator", ledger),
    )
    .with_demand(ledger, demand::no_merge_probe());
    let in_progress = new_in_progress_guard();
    let mut report = TickReport::default();
    let records = crate::test_log_capture::capture_logs(|| {
        report = curator.dispatch_tick(roots.clone(), &in_progress);
    });
    gate.release(usize::MAX);
    drain(&rt, &mut curator);
    assert_eq!(report.spawned.len(), 2, "ceiling 4 − champion's want of 2");
    assert_eq!(report.deferred, 3);
    let lines: Vec<_> = records
        .iter()
        .filter(|(l, m)| *l == log::Level::Warn && m.contains("stopped admitting"))
        .collect();
    assert_eq!(lines.len(), 1, "{records:?}");
    assert!(
        lines[0]
            .1
            .contains("2 of the host ceiling of 4 reserved for champion"),
        "{}",
        lines[0].1
    );
    let info: Vec<_> = records
        .iter()
        .filter(|(l, m)| *l == log::Level::Info && m.contains("demand admission"))
        .collect();
    assert_eq!(info.len(), 1, "the width line is edge-triggered: {records:?}");
}
