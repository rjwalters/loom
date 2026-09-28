//! Tests for the demand ledger, width and Champion-first reservation (#9392).
//!
//! Every test uses its own [`DemandLedger`]; none touches the global one or
//! the network.

use super::*;

fn cfg() -> DemandConfig {
    DemandConfig::default()
}

fn root(n: u8) -> PathBuf {
    PathBuf::from(format!("/tmp/loom-9392-demand-{n}"))
}

fn row(number: u32, is_pull_request: bool) -> RestIssue {
    RestIssue {
        number,
        title: None,
        labels: Vec::new(),
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: "open".to_string(),
        body: None,
        author: None,
        is_pull_request,
    }
}

fn workspace(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(tmp.path().join(".loom").join("config.json"), config).unwrap();
    tmp
}

const HOUR: Duration = Duration::from_secs(3600);

// -- ledger ------------------------------------------------------------------

#[test]
fn ledger_sums_counts_and_roots_with_debt_per_axis() {
    let ledger = DemandLedger::default();
    ledger.record(&root(1), DebtAxis::Merge, 4);
    ledger.record(&root(2), DebtAxis::Merge, 0);
    ledger.record(&root(3), DebtAxis::Merge, 2);
    ledger.record(&root(1), DebtAxis::Review, 5);
    let debt = ledger.host_debt(HOUR);
    assert_eq!(
        debt.merge,
        Some(AxisDebt {
            total: 6,
            roots_with_debt: 2
        }),
        "a zero entry is observed but is not a root with debt"
    );
    assert_eq!(
        debt.review,
        Some(AxisDebt {
            total: 5,
            roots_with_debt: 1
        })
    );
    assert_eq!(debt.changes, None, "an axis nobody recorded is unobserved");
    // A later record replaces the root's entry rather than adding to it.
    ledger.record(&root(1), DebtAxis::Merge, 1);
    assert_eq!(ledger.host_debt(HOUR).merge.unwrap().total, 3);
}

#[test]
fn ledger_entries_older_than_stale_secs_are_unobserved() {
    let ledger = DemandLedger::default();
    let t0 = Instant::now();
    ledger.record_at(&root(1), DebtAxis::Review, 7, t0);
    ledger.record_at(&root(2), DebtAxis::Merge, 3, t0 + Duration::from_secs(1000));
    let stale = cfg().stale();
    let at = |secs| ledger.host_debt_at(t0 + Duration::from_secs(secs), stale);
    assert_eq!(at(1800).review.unwrap().total, 7, "exactly staleSecs old is fresh");
    let later = at(1801);
    assert_eq!(later.review, None, "one axis stale ⇒ that axis is None");
    assert_eq!(later.merge.unwrap().total, 3, "the fresh axis is unaffected");
    assert_eq!(at(2801), HostDebt::default(), "every axis stale ⇒ all None");
}

#[test]
fn a_failed_listing_records_nothing() {
    let ledger = DemandLedger::default();
    let failing: DemandProbe = Arc::new(|_| Err("gh: 502".to_string()));
    let ws = workspace("{}");
    record_merge_debt(&failing, &ledger, ws.path());
    assert_eq!(ledger.host_debt(HOUR).merge, None, "an error never reads as 0");
    let ok: DemandProbe = Arc::new(|_| Ok(0));
    record_merge_debt(&ok, &ledger, ws.path());
    assert_eq!(ledger.host_debt(HOUR).merge, Some(AxisDebt::default()));
}

#[test]
fn only_open_pr_rows_are_counted() {
    let mut closed = row(4, true);
    closed.state = "closed".to_string();
    let rows = vec![row(1, true), row(2, false), row(3, true), closed];
    assert_eq!(count_pr_rows(&rows), 2);
    let ledger = DemandLedger::default();
    let ws = workspace("{}");
    record_listing(&ledger, ws.path(), "loom:review-requested", &rows);
    record_listing(&ledger, ws.path(), "loom:curated", &rows);
    let debt = ledger.host_debt(HOUR);
    assert_eq!(debt.review.unwrap().total, 2, "issue rows are not review debt");
    assert_eq!((debt.changes, debt.merge), (None, None), "an unmapped label feeds nothing");
}

#[test]
fn listings_record_nothing_when_demand_width_is_disabled() {
    let ws = workspace(r#"{"autonomous":{"roleRunner":{"demandWidth":{"enabled":false}}}}"#);
    let ledger = DemandLedger::default();
    record_listing(&ledger, ws.path(), "loom:changes-requested", &[row(1, true)]);
    let calls = Arc::new(AtomicUsize::new(0));
    let probe: DemandProbe = {
        let calls = Arc::clone(&calls);
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(9)
        })
    };
    record_merge_debt(&probe, &ledger, ws.path());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no champion listing when disabled");
    assert_eq!(ledger.host_debt(HOUR), HostDebt::default());
}

#[test]
fn a_root_that_leaves_the_registry_stops_counting() {
    let ledger = DemandLedger::default();
    ledger.record(&root(1), DebtAxis::Merge, 5);
    ledger.record(&root(2), DebtAxis::Merge, 5);
    ledger.retain_roots(&[root(2)]);
    assert_eq!(
        ledger.host_debt(HOUR).merge,
        Some(AxisDebt {
            total: 5,
            roots_with_debt: 1
        })
    );
}

// -- config ------------------------------------------------------------------

#[test]
fn demand_config_defaults_and_per_key_fallback() {
    assert_eq!(parse_demand_config(&serde_json::Value::Null), cfg());
    let parsed = parse_demand_config(&serde_json::json!({
        "demandWidth": {
            "enabled": false,
            "perRun": 5,
            "max": 0,
            "reserve": "yes",
            "nonPrFloor": -1,
            "staleSecs": 60.5
        }
    }));
    assert_eq!(
        parsed,
        DemandConfig {
            enabled: false,
            per_run: 5,
            ..cfg()
        },
        "zero, negative, non-integer and non-bool values drop to the default per key"
    );
    let full = parse_demand_config(&serde_json::json!({
        "demandWidth": {"perRun": 2, "max": 6, "reserve": false, "nonPrFloor": 2, "staleSecs": 90}
    }));
    assert_eq!(
        full,
        DemandConfig {
            enabled: true,
            per_run: 2,
            max: 6,
            reserve: false,
            non_pr_floor: 2,
            stale_secs: 90
        }
    );
    let ws = workspace(r#"{"autonomous":{"roleRunner":{"demandWidth":{"perRun":7}}}}"#);
    assert_eq!(read_demand_config(ws.path()).per_run, 7, "read from the root's config");
}

// -- width -------------------------------------------------------------------

#[test]
fn width_follows_debt_over_per_run_clamped_to_max_and_budget() {
    let c = cfg(); // k = 3, max = 4
    assert_eq!(width(Some(0), &c, 3), 1, "debt 0 → 1");
    assert_eq!(width(Some(3), &c, 3), 1, "debt k → 1");
    assert_eq!(width(Some(4), &c, 3), 2, "debt k+1 → 2");
    assert_eq!(width(Some(1000), &c, 3), 3, "clamped to the Phase 1 budget");
    assert_eq!(width(Some(1000), &c, 7), 4, "clamped to max");
    assert_eq!(width(None, &c, 3), 3, "unobserved → Phase 1 budget");
    assert_eq!(width(None, &c, 7), 7, "unobserved is not clamped to max");
    assert_eq!(width(Some(1000), &c, 2), 2, "roleMaxConcurrent below max binds");
}

#[test]
fn want_counts_only_roots_with_debt() {
    let c = cfg();
    let d = |total, roots_with_debt| {
        Some(AxisDebt {
            total,
            roots_with_debt,
        })
    };
    assert_eq!(want(d(30, 1), &c, 3), 1, "one repo with debt: one run is all it can use");
    assert_eq!(want(d(30, 5), &c, 3), 3, "width binds");
    assert_eq!(want(d(4, 5), &c, 3), 2);
    assert_eq!(want(d(0, 0), &c, 3), 0, "no debt, no want");
    assert_eq!(want(None, &c, 3), 0, "unobserved, no want");
}

#[test]
fn decide_uses_width_for_judge_and_doctor_but_never_lowers_champion() {
    let host = HostDebt {
        review: Some(AxisDebt {
            total: 2,
            roots_with_debt: 2,
        }),
        changes: Some(AxisDebt {
            total: 0,
            roots_with_debt: 0,
        }),
        merge: Some(AxisDebt {
            total: 1,
            roots_with_debt: 1,
        }),
    };
    let budgets = BTreeMap::new();
    let judge = decide("judge", &budgets, 7, &host, &cfg());
    assert_eq!((judge.phase1_budget, judge.width, judge.budget), (3, Some(1), 1));
    assert_eq!(decide("doctor", &budgets, 7, &host, &cfg()).budget, 1);
    let champion = decide("champion", &budgets, 7, &host, &cfg());
    assert_eq!((champion.width, champion.budget), (Some(1), 3), "champion budget unchanged");
    assert_eq!(
        champion.plan,
        ReservationPlan {
            wants: [0; 3],
            cap: 6
        }
    );
    let curator = decide("curator", &budgets, 7, &host, &cfg());
    assert_eq!((curator.width, curator.budget), (None, 3));
    assert_eq!(curator.plan.wants, [1, 1, 0], "doctor has no debt so wants nothing");
    let unobserved = decide("judge", &budgets, 7, &HostDebt::default(), &cfg());
    assert_eq!(unobserved.budget, 3, "unobserved ⇒ Phase 1 budget");
    let no_reserve = DemandConfig {
        reserve: false,
        ..cfg()
    };
    let kept = decide("curator", &budgets, 7, &host, &no_reserve);
    assert_eq!(kept.plan, ReservationPlan::NONE, "reserve:false holds nothing");
    assert_eq!(decide("judge", &budgets, 7, &host, &no_reserve).budget, 1, "width kept");
}

// -- reservation (lock level) ------------------------------------------------

fn spread_merge_debt() -> HostDebt {
    HostDebt {
        merge: Some(AxisDebt {
            total: 9,
            roots_with_debt: 3,
        }),
        ..HostDebt::default()
    }
}

fn plan_for(role: &str, host: &HostDebt, ceiling: usize) -> ReservationPlan {
    let budgets = BTreeMap::new();
    let budget_of =
        |r: &str| concurrent_dispatch::resolve_role_max_concurrent(&budgets, r, ceiling);
    ReservationPlan::for_role(role, host, &cfg(), &budget_of, ceiling)
}

fn fill(set: &InProgressGuard, role: &'static str, n: u8) -> Vec<RoleRunGuard> {
    (0..n)
        .map(|i| {
            RoleRunGuard::admit(set.clone(), root(100 + i), role, usize::MAX)
                .into_guard()
                .unwrap()
        })
        .collect()
}

/// Merge debt over 3 roots with `active_total = ceiling − 1`: a curator is
/// refused with `ReservationHeld` while champion is admitted.
#[test]
fn reservation_refuses_curator_but_admits_champion_one_below_the_ceiling() {
    let set = new_in_progress_guard();
    let _others = fill(&set, "auditor", 3); // ceiling 4, active 3
    let host = spread_merge_debt();
    match RoleRunGuard::admit_with_demand(
        set.clone(),
        root(1),
        "curator",
        4,
        2,
        &plan_for("curator", &host, 4),
    ) {
        RoleAdmission::ReservationHeld {
            active,
            ceiling,
            reserved,
            held_for,
        } => {
            assert_eq!((active, ceiling, reserved), (3, 4, 2));
            assert_eq!(held_for.to_string(), "champion");
        }
        other => panic!("expected ReservationHeld, got {other:?}"),
    }
    let champion = RoleRunGuard::admit_with_demand(
        set.clone(),
        root(1),
        "champion",
        4,
        2,
        &plan_for("champion", &host, 4),
    );
    assert!(matches!(champion, RoleAdmission::Admitted(_)), "{champion:?}");
    assert_eq!(active_run_count(&set), 4);
}

/// The reservation shrinks as the PR role fills its want, and a running
/// champion frees the non-PR role again.
#[test]
fn reservation_counts_only_unfilled_wants() {
    let set = new_in_progress_guard();
    let host = spread_merge_debt(); // champion wants min(ceil(9/3)=3→budget 3, 3) = 3
    let plan = plan_for("curator", &host, 7);
    assert_eq!(plan.wants, [3, 0, 0]);
    let _champions = fill(&set, "champion", 2);
    let _auditors = fill(&set, "auditor", 3);
    // active 5, reserved 1 → 6 < 7: admitted.
    let admitted = RoleRunGuard::admit_with_demand(set.clone(), root(1), "curator", 7, 3, &plan);
    assert!(matches!(admitted, RoleAdmission::Admitted(_)), "{admitted:?}");
    // active 6, reserved 1 → 7: refused.
    assert!(matches!(
        RoleRunGuard::admit_with_demand(set.clone(), root(2), "hermit", 7, 3, &plan),
        RoleAdmission::ReservationHeld { reserved: 1, .. }
    ));
}

#[test]
fn reservation_never_exceeds_ceiling_minus_non_pr_floor() {
    let host = HostDebt {
        review: Some(AxisDebt {
            total: 100,
            roots_with_debt: 10,
        }),
        changes: Some(AxisDebt {
            total: 100,
            roots_with_debt: 10,
        }),
        merge: Some(AxisDebt {
            total: 100,
            roots_with_debt: 10,
        }),
    };
    let plan = plan_for("curator", &host, 7);
    assert_eq!(plan.wants, [3, 3, 3]);
    assert_eq!((plan.cap, plan.planned()), (6, 6), "Σ want 9 capped at 7 − 1");
    let (reserved, held) = plan.reserved(|_| 0);
    assert_eq!(reserved, 6);
    assert_eq!(held.to_string(), "champion+judge+doctor");
    // A non-PR role still gets its floor slot on an otherwise idle host.
    let set = new_in_progress_guard();
    let curator = RoleRunGuard::admit_with_demand(set.clone(), root(1), "curator", 7, 3, &plan);
    assert!(matches!(curator, RoleAdmission::Admitted(_)));
    // Judge is held back only by champion's unfilled want.
    assert_eq!(plan_for("judge", &host, 7).wants, [3, 0, 0]);
    assert_eq!(plan_for("doctor", &host, 7).wants, [3, 3, 0]);
}

/// Ceiling 1 with `nonPrFloor` 1: the cap is 0, so champion and curator can
/// each still run (one at a time).
#[test]
fn ceiling_one_reserves_nothing() {
    let plan = plan_for("curator", &spread_merge_debt(), 1);
    assert_eq!(plan.planned(), 0);
    let set = new_in_progress_guard();
    let curator = RoleRunGuard::admit_with_demand(set.clone(), root(1), "curator", 1, 1, &plan);
    assert!(matches!(curator, RoleAdmission::Admitted(_)));
    drop(curator);
    let champion = RoleRunGuard::admit_with_demand(
        set,
        root(1),
        "champion",
        1,
        1,
        &plan_for("champion", &spread_merge_debt(), 1),
    );
    assert!(matches!(champion, RoleAdmission::Admitted(_)));
}

#[test]
fn reservation_is_zero_when_every_axis_is_unobserved() {
    let plan = plan_for("curator", &HostDebt::default(), 7);
    assert_eq!(plan.wants, [0; 3]);
    assert_eq!(plan.reserved(|_| 0), (0, HeldFor::default()));
    let set = new_in_progress_guard();
    let _full = fill(&set, "auditor", 6);
    assert!(matches!(
        RoleRunGuard::admit_with_demand(set, root(1), "curator", 7, 3, &plan),
        RoleAdmission::Admitted(_)
    ));
}

/// The same lock order as Phase 1: `InProgress`, then the ceiling, then the
/// (effective) budget, and only then the reservation.
#[test]
fn admit_with_demand_keeps_the_phase1_refusal_order() {
    let set = new_in_progress_guard();
    let plan = plan_for("judge", &spread_merge_debt(), 3);
    let _j = RoleRunGuard::admit_with_demand(set.clone(), root(1), "judge", 3, 1, &plan)
        .into_guard()
        .unwrap();
    assert!(matches!(
        RoleRunGuard::admit_with_demand(set.clone(), root(1), "judge", 3, 1, &plan),
        RoleAdmission::InProgress
    ));
    assert!(matches!(
        RoleRunGuard::admit_with_demand(set.clone(), root(2), "judge", 3, 1, &plan),
        RoleAdmission::RoleBudgetReached {
            active: 1,
            budget: 1,
            ..
        }
    ));
    let _a = fill(&set, "auditor", 2);
    assert!(matches!(
        RoleRunGuard::admit_with_demand(set, root(3), "champion", 3, 2, &ReservationPlan::NONE),
        RoleAdmission::CeilingReached {
            active: 3,
            ceiling: 3
        }
    ));
}

// -- logging -----------------------------------------------------------------

#[test]
fn the_width_line_is_logged_once_per_change() {
    let ledger = DemandLedger::default();
    let budgets = BTreeMap::new();
    let mut host = spread_merge_debt();
    let records = crate::test_log_capture::capture_logs(|| {
        for _ in 0..5 {
            let d = decide("curator", &budgets, 7, &host, &cfg());
            log_if_changed(&ledger, &d, &cfg());
        }
        host.merge = Some(AxisDebt {
            total: 1,
            roots_with_debt: 1,
        });
        let d = decide("curator", &budgets, 7, &host, &cfg());
        log_if_changed(&ledger, &d, &cfg());
    });
    let lines: Vec<_> = records
        .iter()
        .filter(|(l, m)| *l == log::Level::Info && m.contains("demand admission"))
        .collect();
    assert_eq!(lines.len(), 2, "one line per change: {records:?}");
    assert!(lines[0].1.contains("ceiling reservation 3"), "{}", lines[0].1);
    assert!(lines[0].1.contains("phase-1 budget 3"), "{}", lines[0].1);
    assert!(lines[1].1.contains("ceiling reservation 1"), "{}", lines[1].1);
}
