//! Tests for the deterministic per-repository lane rule (#10630): the
//! judge/doctor formula, config knobs, the host-capacity trim order, distinct
//! PR assignment for judge lanes, and admission.

use super::*;
use crate::role_runner::concurrent_dispatch::lanes::{assign, LaneProbe, LaneTarget};
use demand::{DebtAxis, DemandConfig, DemandLedger};
use serde_json::json;
use std::sync::Arc;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn formula_for_both_roles_and_the_acceptance_examples() {
    let cfg = DemandConfig::default();
    let ledger = DemandLedger::default();
    let (hot, cold) = (p("/tmp/loom-10630-hot"), p("/tmp/loom-10630-cold"));
    for axis in [DebtAxis::Changes, DebtAxis::Review] {
        ledger.record(&hot, axis, 60);
        ledger.record(&cold, axis, 2);
    }
    for role in ["doctor", "judge"] {
        let lanes = |r: &Path| demand::repo_lanes(role, &ledger.repo_debt(r, cfg.stale()), &cfg);
        assert_eq!(lanes(&hot), 3, "{role}: 60 PRs gets perRepoCap");
        assert_eq!(lanes(&cold), 1, "{role}: 2 PRs gets one");
    }
    // Other roles never widen.
    let hot_debt = ledger.repo_debt(&hot, cfg.stale());
    assert_eq!(demand::repo_lanes("champion", &hot_debt, &cfg), 1);
}

#[test]
fn judge_follows_its_review_axis_not_changes() {
    let cfg = DemandConfig::default();
    let ledger = DemandLedger::default();
    let root = p("/tmp/loom-10630-axis");
    ledger.record(&root, DebtAxis::Changes, 60);
    ledger.record(&root, DebtAxis::Review, 11);
    let debt = ledger.repo_debt(&root, cfg.stale());
    assert_eq!(demand::repo_lanes("judge", &debt, &cfg), 2);
    assert_eq!(demand::repo_lanes("doctor", &debt, &cfg), 3);
}

#[test]
fn knobs_parse_and_doctor_max_per_repo_stays_a_doctor_override() {
    let parse = |v: serde_json::Value| demand::parse_demand_config(&json!({ "demandWidth": v }));
    let d = parse(json!({}));
    assert_eq!((d.lane_k, d.per_repo_cap, d.doctor_max_per_repo), (10, 3, 3));
    let c = parse(json!({ "laneK": 5, "perRepoCap": 2 }));
    assert_eq!((c.lane_k, c.per_repo_cap, c.doctor_max_per_repo), (5, 2, 2));
    let c = parse(json!({ "perRepoCap": 2, "doctorMaxPerRepo": 4 }));
    assert_eq!((c.per_repo_cap, c.doctor_max_per_repo), (2, 4));
    let c = parse(json!({ "laneK": 0, "perRepoCap": "x" }));
    assert_eq!((c.lane_k, c.per_repo_cap), (10, 3), "bad values drop to defaults");
    assert_eq!(
        parse(json!({ "perRepoCap": 1000 })).per_repo_cap,
        demand::DOCTOR_MAX_PER_REPO_LIMIT
    );
}

fn wishes() -> Vec<(PathBuf, usize, usize)> {
    vec![
        (p("/r/zeta"), 60, 3),
        (p("/r/alpha"), 25, 3),
        (p("/r/beta"), 25, 3),
        (p("/r/small"), 2, 1),
    ]
}

#[test]
fn no_trim_when_under_capacity() {
    let out = allocate(&wishes(), 10);
    assert_eq!(out.iter().map(|r| r.lanes).sum::<usize>(), 10);
    assert!(out.iter().all(|r| r.lanes == r.wanted));
}

#[test]
fn trim_removes_lowest_debt_extras_first_ties_by_name() {
    // wished 3+3+3+1 = 10; capacity 8 trims two lanes: the tie at debt 25 falls
    // to the smaller path (alpha) first, and its extras go before beta's.
    let out = allocate(&wishes(), 8);
    let lanes = |n: &str| out.iter().find(|r| r.root == p(n)).unwrap().lanes;
    assert_eq!(
        (lanes("/r/alpha"), lanes("/r/beta"), lanes("/r/zeta"), lanes("/r/small")),
        (1, 3, 3, 1)
    );
    // Capacity 6: alpha and beta both lose their extras; the highest-debt repo
    // keeps its three lanes.
    let out = allocate(&wishes(), 6);
    let lanes = |n: &str| out.iter().find(|r| r.root == p(n)).unwrap().lanes;
    assert_eq!(
        (lanes("/r/alpha"), lanes("/r/beta"), lanes("/r/zeta"), lanes("/r/small")),
        (1, 1, 3, 1)
    );
}

#[test]
fn trim_drops_whole_repositories_lowest_debt_first_only_after_extras() {
    let out = allocate(&wishes(), 2);
    let lanes = |n: &str| out.iter().find(|r| r.root == p(n)).unwrap().lanes;
    assert_eq!(
        (lanes("/r/small"), lanes("/r/alpha"), lanes("/r/beta"), lanes("/r/zeta")),
        (0, 0, 1, 1),
        "small (debt 2) then alpha (tie, smaller name) are dropped"
    );
    assert_eq!(
        allocate(&wishes(), 0)
            .iter()
            .map(|r| r.lanes)
            .sum::<usize>(),
        0
    );
}

#[test]
fn allocation_is_independent_of_input_order() {
    let forward = allocate(&wishes(), 5);
    let mut reversed = wishes();
    reversed.reverse();
    assert_eq!(forward, allocate(&reversed, 5));
    assert!(forward.windows(2).all(|w| w[0].root < w[1].root), "sorted by root");
}

#[test]
fn plan_for_reads_only_the_roles_axis_and_lanes_for_defaults_to_one() {
    let cfg = DemandConfig::default();
    let ledger = DemandLedger::default();
    let (a, b) = (p("/tmp/loom-10630-plan-a"), p("/tmp/loom-10630-plan-b"));
    ledger.record(&a, DebtAxis::Review, 60);
    ledger.record(&b, DebtAxis::Changes, 60);
    let plan = plan_for("judge", &ledger, &cfg, 8);
    assert_eq!(plan.len(), 1, "b has no review observation");
    assert_eq!((plan[0].debt, plan[0].wanted, plan[0].lanes), (60, 3, 3));
    assert_eq!(lanes_for(&plan, &a, 1), 3);
    assert_eq!(lanes_for(&plan, &b, 1), 1, "unknown root keeps the wish");
    assert!(plan_for("champion", &ledger, &cfg, 8).is_empty());
    // Trimmed to zero still leaves the classic first lane.
    let squeezed = plan_for("judge", &ledger, &cfg, 0);
    assert_eq!(lanes_for(&squeezed, &a, 3), 1);
}

#[test]
fn concurrent_judge_and_doctor_lanes_never_take_the_same_pr() {
    let root = p("/tmp/loom-10630-distinct");
    for role in ["judge", "doctor"] {
        let probe = LaneProbe {
            queue: Arc::new(|_, _| Ok(vec![1, 2, 3, 4])),
            verdict: Arc::new(|_, _| true),
        };
        let mut held = Vec::new();
        for lane in 0..4 {
            let LaneTarget::Pr(h) = assign(&probe, role, &root, lane) else {
                panic!("{role} lane {lane} should get a PR");
            };
            held.push(h);
        }
        let mut prs: Vec<u64> = held.iter().map(|h| h.pr()).collect();
        prs.sort_unstable();
        assert_eq!(prs, vec![1, 2, 3, 4], "{role}: four lanes, four distinct PRs");
        assert!(matches!(assign(&probe, role, &root, 4), LaneTarget::Nothing));
        drop(held);
    }
}

#[test]
fn judge_lanes_skip_the_doctor_only_verdict_guard() {
    let root = p("/tmp/loom-10630-verdict");
    let probe = LaneProbe {
        queue: Arc::new(|_, role| {
            assert_eq!(role, "judge");
            Ok(vec![7])
        }),
        verdict: Arc::new(|_, _| false),
    };
    assert!(matches!(assign(&probe, "judge", &root, 1), LaneTarget::Pr(_)));
}
