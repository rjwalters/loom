//! Tests for the per-repo balance allocator, shadow mode (#10630 Slice 1).
//!
//! Pure: every test builds its own pipelines / ledger; none touches the
//! global ledger, the process env, or the network.

use super::*;
use crate::types::QueueDisposition as Qd;
use crate::work_finder::PriorityCandidate;

fn root(name: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/loom-10630-balance/{name}"))
}

fn pipe(review: usize, changes: usize, merge: usize, ready: usize) -> RepoPipeline {
    RepoPipeline {
        review: Some(review),
        changes: Some(changes),
        merge: Some(merge),
        ready: Some(ready),
        ..RepoPipeline::default()
    }
}

fn slots(allocs: &[Allocation], repo: &str, role: &str) -> usize {
    allocs
        .iter()
        .find(|a| a.root == root(repo) && a.role == role)
        .map(|a| a.slots)
        .unwrap()
}

fn find<'a>(allocs: &'a [Allocation], repo: &str, role: &str) -> &'a Allocation {
    allocs
        .iter()
        .find(|a| a.root == root(repo) && a.role == role)
        .unwrap()
}

fn total(allocs: &[Allocation]) -> usize {
    allocs.iter().map(|a| a.slots).sum()
}

fn cfg_w(w: f64) -> AllocatorConfig {
    AllocatorConfig {
        review_weight: w,
        ..AllocatorConfig::default()
    }
}

#[test]
fn changes_heavy_repo_gets_doctors_and_no_builders() {
    let repos = vec![(root("a"), pipe(0, 12, 0, 10))];
    let allocs = allocate(&repos, 8, &AllocatorConfig::default());
    // ceil(12 / perRun 3) = 4, clamped at max 4.
    assert_eq!(slots(&allocs, "a", "doctor"), 4);
    assert_eq!(slots(&allocs, "a", "builder"), 0);
    let b = find(&allocs, "a", "builder");
    assert_eq!(b.trigger, Trigger::Debt(DebtAxis::Changes));
    assert!(b.reason.contains("builds paused"), "{}", b.reason);
    assert_eq!(find(&allocs, "a", "doctor").trigger, Trigger::Debt(DebtAxis::Changes));
}

#[test]
fn review_heavy_repo_gets_judges() {
    let repos = vec![
        (root("busy"), pipe(9, 0, 0, 0)),
        (root("quiet"), pipe(0, 0, 0, 0)),
    ];
    let allocs = allocate(&repos, 6, &AllocatorConfig::default());
    assert_eq!(slots(&allocs, "busy", "judge"), 3);
    assert_eq!(slots(&allocs, "quiet", "judge"), 0);
    assert_eq!(find(&allocs, "busy", "judge").trigger, Trigger::Debt(DebtAxis::Review));
}

#[test]
fn ready_repo_without_debt_gets_builders() {
    let repos = vec![(root("a"), pipe(0, 0, 0, 5))];
    let allocs = allocate(&repos, 3, &AllocatorConfig::default());
    assert_eq!(slots(&allocs, "a", "builder"), 3);
    assert_eq!(find(&allocs, "a", "builder").trigger, Trigger::Ready);
    assert_eq!(slots(&allocs, "a", "judge"), 0);
    assert_eq!(slots(&allocs, "a", "doctor"), 0);
    assert_eq!(slots(&allocs, "a", "champion"), 0);
}

#[test]
fn unobserved_axes_fail_open_with_a_floor() {
    let repos = vec![(root("a"), RepoPipeline::default())];
    let allocs = allocate(&repos, 10, &AllocatorConfig::default());
    for role in ROLES {
        let a = find(&allocs, "a", role);
        assert_eq!(a.slots, 1, "{role} must not be starved by missing data");
        assert_eq!(a.trigger, Trigger::Floor, "{role}");
    }
}

#[test]
fn champion_goes_first_when_budget_is_tight() {
    let repos = vec![
        (root("a"), pipe(30, 30, 2, 50)),
        (root("b"), pipe(0, 0, 5, 50)),
    ];
    let cfg = AllocatorConfig {
        non_pr_floor: 0,
        ..AllocatorConfig::default()
    };
    let allocs = allocate(&repos, 2, &cfg);
    assert_eq!(slots(&allocs, "a", "champion"), 1);
    assert_eq!(slots(&allocs, "b", "champion"), 1);
    assert_eq!(total(&allocs), 2);
    // The default nonPrFloor of 1 leaves one slot for the marginal pass,
    // and the deepest merge queue is served first.
    let allocs = allocate(&repos, 2, &AllocatorConfig::default());
    assert_eq!(slots(&allocs, "b", "champion"), 1);
    assert_eq!(total(&allocs), 2);
}

#[test]
fn allocator_config_carries_demand_reserve() {
    let on = AllocatorConfig::new(&BalanceConfig::default(), &DemandConfig::default());
    assert!(on.reserve, "demandWidth.reserve defaults to true");
    let off = AllocatorConfig::new(
        &BalanceConfig::default(),
        &DemandConfig {
            reserve: false,
            ..DemandConfig::default()
        },
    );
    assert!(!off.reserve);
    assert_eq!(
        (off.per_run, off.max_per_repo, off.non_pr_floor),
        (on.per_run, on.max_per_repo, on.non_pr_floor)
    );
}

#[test]
fn reserve_false_skips_the_champion_first_pass() {
    // Two merge-debt repos, plus review / ready demand that outbids a
    // champion (merge demand is 1 per PR, unweighted) in the marginal pass.
    let repos = vec![
        (root("a"), pipe(30, 0, 1, 0)),
        (root("b"), pipe(0, 0, 1, 2)),
    ];
    let reserving = AllocatorConfig {
        non_pr_floor: 0,
        reserve: true,
        ..AllocatorConfig::default()
    };
    let not_reserving = AllocatorConfig {
        reserve: false,
        ..reserving
    };
    for budget in [0, 1, 2, 3, 6, 20] {
        let on = allocate(&repos, budget, &reserving);
        let off = allocate(&repos, budget, &not_reserving);
        assert!(total(&on) <= budget, "reserve=true budget {budget}");
        assert!(total(&off) <= budget, "reserve=false budget {budget}");
    }
    // reserve=true: both champions are preallocated before any judge/builder.
    let on = allocate(&repos, 2, &reserving);
    assert_eq!((slots(&on, "a", "champion"), slots(&on, "b", "champion")), (1, 1));
    assert_eq!(total(&on), 2);
    // reserve=false: no preallocation — the two slots go by marginal value
    // (judge demand 30 outbids builder 2 and champion 1), so no champion gets one.
    let off = allocate(&repos, 2, &not_reserving);
    assert_eq!((slots(&off, "a", "champion"), slots(&off, "b", "champion")), (0, 0));
    assert_eq!(slots(&off, "a", "judge") + slots(&off, "b", "builder"), 2);
    assert_eq!(total(&off), 2);
    // Width/demand allocation is otherwise intact: with room to spare, the
    // champions still win their slot in the marginal pass.
    let off = allocate(&repos, 20, &not_reserving);
    assert_eq!((slots(&off, "a", "champion"), slots(&off, "b", "champion")), (1, 1));
    assert_eq!(slots(&off, "a", "judge"), 4);
}

#[test]
fn total_never_exceeds_the_budget() {
    let repos: Vec<_> = (0..12)
        .map(|i| (root(&format!("r{i:02}")), pipe(i * 3, i * 2, i, 20)))
        .collect();
    for reserve in [true, false] {
        let cfg = AllocatorConfig {
            reserve,
            ..AllocatorConfig::default()
        };
        for budget in [0, 1, 3, 7, 15, 40, 500] {
            let allocs = allocate(&repos, budget, &cfg);
            assert!(total(&allocs) <= budget, "budget {budget} reserve {reserve}");
            assert_eq!(allocs.len(), repos.len() * ROLES.len());
        }
    }
    assert_eq!(total(&allocate(&repos, 0, &AllocatorConfig::default())), 0);
}

#[test]
fn allocation_is_deterministic_whatever_the_input_order() {
    let mut repos = vec![
        (root("c"), pipe(4, 1, 1, 3)),
        (root("a"), pipe(4, 1, 1, 3)),
        (root("b"), pipe(4, 1, 1, 3)),
    ];
    let first = allocate(&repos, 5, &AllocatorConfig::default());
    repos.reverse();
    let second = allocate(&repos, 5, &AllocatorConfig::default());
    assert_eq!(first, second);
    let order: Vec<_> = first.iter().map(|a| (a.root.clone(), a.role)).collect();
    let mut sorted = order.clone();
    sorted.sort_by(|x, y| {
        x.0.cmp(&y.0).then(
            ROLES
                .iter()
                .position(|r| *r == x.1)
                .cmp(&ROLES.iter().position(|r| *r == y.1)),
        )
    });
    assert_eq!(order, sorted, "ordered by path then role");
    // Equal repos tie-break by path: with one judge slot to spare, `a` wins.
    let allocs = allocate(&repos, 1, &AllocatorConfig::default());
    assert_eq!(slots(&allocs, "a", "judge"), 1);
}

#[test]
fn review_weight_tilts_review_versus_build() {
    let repos = vec![(root("a"), pipe(3, 0, 0, 10))];
    let even = allocate(&repos, 3, &cfg_w(1.0));
    assert_eq!((slots(&even, "a", "judge"), slots(&even, "a", "builder")), (1, 2));
    let review = allocate(&repos, 3, &cfg_w(4.0));
    assert_eq!((slots(&review, "a", "judge"), slots(&review, "a", "builder")), (1, 0));
    let build = allocate(&repos, 3, &cfg_w(0.25));
    assert_eq!((slots(&build, "a", "judge"), slots(&build, "a", "builder")), (0, 3));
}

#[test]
fn config_defaults_off_and_falls_back_on_bad_values() {
    let d = BalanceConfig::resolve(&Value::Null, None, None);
    assert_eq!(d, BalanceConfig::default());
    assert!(!d.enabled);
    assert!((d.review_weight - 1.0).abs() < f64::EPSILON);

    let block = serde_json::json!({"enabled": true, "reviewWeight": 2.5});
    let c = BalanceConfig::resolve(&block, None, None);
    assert!(c.enabled);
    assert!((c.review_weight - 2.5).abs() < f64::EPSILON);

    for bad in [
        serde_json::json!({"enabled": "yes", "reviewWeight": 0}),
        serde_json::json!({"enabled": 1, "reviewWeight": -2.0}),
        serde_json::json!({"reviewWeight": "heavy"}),
    ] {
        let c = BalanceConfig::resolve(&bad, None, None);
        assert!(!c.enabled, "{bad}");
        assert!((c.review_weight - DEFAULT_REVIEW_WEIGHT).abs() < f64::EPSILON, "{bad}");
    }
}

#[test]
fn env_beats_config_and_bad_env_falls_through() {
    let block = serde_json::json!({"enabled": false, "reviewWeight": 2.0});
    let c = BalanceConfig::resolve(&block, Some("true"), Some("0.5"));
    assert!(c.enabled);
    assert!((c.review_weight - 0.5).abs() < f64::EPSILON);
    let c = BalanceConfig::resolve(&block, Some("maybe"), Some("NaN"));
    assert!(!c.enabled);
    assert!((c.review_weight - 2.0).abs() < f64::EPSILON);
    let c = BalanceConfig::resolve(&Value::Null, None, Some("-1"));
    assert!((c.review_weight - DEFAULT_REVIEW_WEIGHT).abs() < f64::EPSILON);
}

fn row(ws: usize, n: u32, d: Option<Qd>) -> TickQueueRow {
    TickQueueRow {
        key: PriorityCandidate {
            workspace_idx: ws,
            number: n,
            ..PriorityCandidate::default()
        },
        tier: None,
        disposition: d,
        detail: None,
        updated_at: None,
        held_until: None,
        story_points: None,
    }
}

#[test]
fn queue_counts_ready_and_building_per_root() {
    let queue = vec![
        row(0, 1, Some(Qd::Dispatched)),
        row(0, 2, Some(Qd::DeferredBuildBackoff)),
        row(0, 3, Some(Qd::InFlight)),
        row(0, 4, Some(Qd::Parked)),
        row(1, 5, None),
        row(2, 6, Some(Qd::WorkspaceHalted)),
        row(9, 7, Some(Qd::Dispatched)),
    ];
    let counts = queue_counts(&queue, 3, &[false, false, true], &[]);
    // A halted root admits nothing: ready is a known zero, building unobserved.
    assert_eq!(counts, vec![(Some(2), Some(1)), (Some(1), Some(0)), (Some(0), None)]);
}

#[test]
fn failed_listing_stays_unobserved_and_gets_the_builder_floor() {
    // Root 0: listing failed (no rows). Root 1: listed successfully, empty.
    // Root 2: halted (listing also failed, but halt is a known zero by policy).
    let counts = queue_counts(&[], 3, &[false, false, true], &[0, 2]);
    assert_eq!(counts, vec![(None, None), (Some(0), Some(0)), (Some(0), None)]);

    let ledger = DemandLedger::default();
    let (a, b) = (root("a"), root("b"));
    for r in [&a, &b] {
        ledger.record(r, DebtAxis::Review, 0);
        ledger.record(r, DebtAxis::Changes, 0);
        ledger.record(r, DebtAxis::Merge, 0);
    }
    let enabled = BalanceConfig {
        enabled: true,
        ..BalanceConfig::default()
    };
    let allocs = shadow_allocation(
        &enabled,
        &DemandConfig::default(),
        &ledger,
        &[a, b],
        &[],
        &[false, false],
        &[0],
        4,
    )
    .unwrap();
    assert_eq!(slots(&allocs, "a", "builder"), 1, "failed listing -> unobserved floor");
    assert_eq!(slots(&allocs, "b", "builder"), 0, "successful empty listing -> no ready issues");
}

#[test]
fn halted_repo_gets_no_builder_floor() {
    let halted = RepoPipeline {
        ready: Some(0),
        ..pipe(0, 0, 0, 0)
    };
    let allocs = allocate(&[(root("a"), halted)], 5, &AllocatorConfig::default());
    assert_eq!(slots(&allocs, "a", "builder"), 0);
    assert_eq!(find(&allocs, "a", "builder").trigger, Trigger::Ready);
}

#[test]
fn unobserved_ready_paused_reason_names_no_ready_count() {
    let p = RepoPipeline {
        review: Some(2),
        changes: Some(0),
        merge: Some(0),
        ..RepoPipeline::default()
    };
    let allocs = allocate(&[(root("a"), p)], 5, &AllocatorConfig::default());
    let b = find(&allocs, "a", "builder");
    assert_eq!(b.slots, 0);
    assert!(b.reason.contains("ready queue unobserved"), "{}", b.reason);
    assert!(!b.reason.contains("ready 1"), "{}", b.reason);
}

#[test]
fn disabled_computes_nothing_and_reads_no_ledger() {
    let ledger = DemandLedger::default();
    let roots = vec![root("a")];
    let out = shadow_allocation(
        &BalanceConfig::default(),
        &DemandConfig::default(),
        &ledger,
        &roots,
        &[],
        &[false],
        &[],
        10,
    );
    assert!(out.is_none());
    assert_eq!(ledger.reads(), 0);
}

#[test]
fn enabled_reads_each_repo_debt_from_the_ledger() {
    let ledger = DemandLedger::default();
    let (a, b) = (root("a"), root("b"));
    ledger.record(&a, DebtAxis::Changes, 9);
    ledger.record(&a, DebtAxis::Review, 0);
    ledger.record(&a, DebtAxis::Merge, 0);
    ledger.record(&b, DebtAxis::Review, 0);
    ledger.record(&b, DebtAxis::Changes, 0);
    ledger.record(&b, DebtAxis::Merge, 0);
    let roots = vec![a, b];
    let queue = vec![row(0, 1, None), row(1, 2, None), row(1, 3, None)];
    let enabled = BalanceConfig {
        enabled: true,
        ..BalanceConfig::default()
    };
    let allocs = shadow_allocation(
        &enabled,
        &DemandConfig::default(),
        &ledger,
        &roots,
        &queue,
        &[false, false],
        &[],
        6,
    )
    .unwrap();
    assert_eq!(slots(&allocs, "a", "doctor"), 3);
    assert_eq!(slots(&allocs, "a", "builder"), 0);
    assert_eq!(slots(&allocs, "b", "builder"), 2);
    assert_eq!(ledger.reads(), 2, "one repo_debt read per root");

    let line = log_line(&allocs, 6, 1.0);
    let build = crate::telemetry::trace::provenance::daemon();
    assert!(line.contains(build.version) && line.contains(build.revision), "{line}");
    assert!(line.contains("/tmp/loom-10630-balance/a/doctor=3 [debt:changes"), "{line}");
    assert!(line.contains("budget 6, assigned 5"), "{line}");
    assert_eq!(line.lines().count(), 1, "one line per tick");
}
