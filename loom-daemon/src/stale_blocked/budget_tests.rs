//! Tests for the `check-stale-blocked` budget floor and `forge_cost`
//! accounting (#10480), against [`super::batch_tests`]'s counting fake.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::batch::{gather_all, Gathering, Options};
use super::batch_tests::{fleet, issue, row, state, Fake};
use super::budget::{project, refusal, Budget, Floor, ForgeCost, Projection};
use super::{classify, Artifact, Verdict};

fn gather(fake: &mut Fake, floor: Floor) -> Gathering {
    gather_all(
        fake,
        &fleet(),
        Options {
            limit: 1000,
            no_prs: false,
            floor,
        },
    )
}

fn budget(core: u64, graphql: u64) -> Budget {
    Budget {
        core_remaining: core,
        graphql_remaining: graphql,
    }
}

fn no_evidence_reads(fake: &Fake) -> bool {
    fake.comment_calls.is_empty()
        && fake.state_calls.is_empty()
        && fake.merge_calls.is_empty()
        && fake.closing_calls.is_empty()
}

// --- pre-run floor ------------------------------------------------------------

#[test]
fn a_run_that_would_cross_the_graphql_floor_gathers_nothing() {
    let mut fake = Fake {
        budget: Some(budget(5000, 1000)),
        ..Fake::default()
    };
    for n in 1..=3 {
        fake.rows.push(issue(n, "Blocked by #7"));
    }
    let g = gather(&mut fake, Floor::default());
    assert!(no_evidence_reads(&fake), "zero evidence calls under the floor");
    assert_eq!(g.items.len(), 3, "every artifact is still reported");
    for item in &g.items {
        let why = item.evidence.as_ref().unwrap_err();
        assert!(
            why.contains("budget floor: graphql remaining 1000, projected 1, floor 1000"),
            "{why}"
        );
    }
    assert!(g.enumerate_error.is_none(), "a refusal is not an enumeration failure");
    assert_eq!(g.cost.budget_before, Some(budget(5000, 1000)));
    assert!(g.cost.budget_refused.is_some());
    assert_eq!(g.cost.meter.graphql_queries, 0);
}

#[test]
fn a_run_that_would_cross_the_core_floor_gathers_nothing() {
    let mut fake = Fake {
        // 3 issues project 6 core reads: 1005 - 6 = 999 < 1000.
        budget: Some(budget(1005, 5000)),
        ..Fake::default()
    };
    for n in 1..=3 {
        fake.rows.push(issue(n, "Blocked by #7"));
    }
    let g = gather(&mut fake, Floor::default());
    assert!(no_evidence_reads(&fake));
    let why = g.cost.budget_refused.unwrap();
    assert!(why.contains("core remaining 1005, projected 6, floor 1000"), "{why}");
}

#[test]
fn a_floor_of_zero_disables_the_check() {
    let mut fake = Fake {
        budget: Some(budget(3, 0)),
        ..Fake::default()
    };
    fake.rows.push(issue(1, "Blocked by #7"));
    fake.states.insert((None, 7), state("CLOSED"));
    let g = gather(
        &mut fake,
        Floor {
            graphql: 0,
            core: 0,
        },
    );
    assert!(g.cost.budget_refused.is_none());
    assert!(matches!(classify(g.items[0].evidence.as_ref().unwrap()), Verdict::Stale(_)));
}

#[test]
fn a_failed_probe_proceeds_rather_than_vanishing() {
    let mut fake = Fake::default(); // `budget: None` — the probe did not answer.
    fake.rows.push(issue(1, "Blocked by #7"));
    fake.states.insert((None, 7), state("OPEN"));
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.budget_calls, 1);
    assert!(g.cost.budget_before.is_none());
    assert!(g.cost.budget_refused.is_none());
    assert!(g.items[0].evidence.is_ok());
}

#[test]
fn an_open_breaker_refuses_without_probing() {
    let mut fake = Fake {
        breaker: true,
        budget: Some(budget(5000, 5000)),
        ..Fake::default()
    };
    fake.rows.push(issue(1, "Blocked by #7"));
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.budget_calls, 0);
    assert!(no_evidence_reads(&fake));
    assert!(g.items[0]
        .evidence
        .as_ref()
        .unwrap_err()
        .contains("breaker"));
}

#[test]
fn a_pr_only_run_is_not_refused_on_the_graphql_bucket_it_never_touches() {
    let mut fake = Fake {
        budget: Some(budget(5000, 10)),
        ..Fake::default()
    };
    fake.rows
        .push(row(50, "nothing cited", 0, true, &["loom:blocked"]));
    let g = gather(&mut fake, Floor::default());
    assert!(g.cost.budget_refused.is_none());
    assert_eq!(classify(g.items[0].evidence.as_ref().unwrap()), Verdict::Undocumented);
}

#[test]
fn an_empty_population_is_never_probed() {
    let mut fake = Fake::default();
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.budget_calls, 0);
    assert_eq!(g.cost.projected, Projection::default());
}

// --- mid-run stop --------------------------------------------------------------

#[test]
fn a_low_graphql_reading_mid_run_skips_the_remaining_batches() {
    let mut fake = Fake {
        budget: Some(budget(5000, 5000)),
        ..Fake::default()
    };
    for n in 1..=250 {
        fake.rows.push(issue(n, "nothing cited"));
    }
    // The first batch reports 999 left: below the 1000 floor.
    fake.graphql_remaining.push_back(999);
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.closing_calls, vec![100], "batches 2 and 3 are never sent");
    let evaluated = g.items.iter().filter(|i| i.evidence.is_ok()).count();
    assert_eq!(evaluated, 100);
    let skipped = g.items.iter().find(|i| i.number == 101).unwrap();
    let why = skipped.evidence.as_ref().unwrap_err();
    assert!(
        why.contains("budget floor reached mid-run: graphql remaining 999 < floor 1000"),
        "{why}"
    );
    assert!(g
        .cost
        .budget_stopped
        .unwrap()
        .contains("graphql remaining 999"));
    assert_eq!(g.cost.meter.graphql_queries, 1);
}

#[test]
fn a_low_core_reading_mid_run_stops_the_remaining_reads() {
    let mut fake = Fake::default();
    fake.rows.push(issue(1, "Blocked by #7"));
    fake.rows.push(issue(2, "Blocked by #8"));
    fake.rows.push(issue(3, "Blocked by #9"));
    for b in 7..=9 {
        fake.states.insert((None, b), state("CLOSED"));
    }
    // The first state read leaves plenty; the second drops below the floor.
    fake.core_remaining.extend([5000, 999]);
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.state_calls.len(), 2, "the third blocker is never read");
    let find = |n: i64| g.items.iter().find(|i| i.number == n).unwrap();
    assert!(find(1).evidence.is_ok());
    assert!(find(2).evidence.is_ok(), "a read already answered still counts");
    let why = find(3).evidence.as_ref().unwrap_err();
    assert!(why.contains("core remaining 999 < floor 1000"), "{why}");
    assert!(g.cost.budget_stopped.is_some());
}

#[test]
fn a_low_core_reading_stops_later_comment_and_state_reads() {
    let mut fake = Fake::default();
    fake.rows
        .push(row(1, "Blocked by #7", 2, false, &["loom:blocked"]));
    fake.rows
        .push(row(2, "Blocked by #7", 2, false, &["loom:blocked"]));
    fake.comments.insert(1, Vec::new());
    fake.core_remaining.push_back(10);
    let g = gather(&mut fake, Floor::default());
    assert_eq!(fake.comment_calls, vec![1]);
    assert!(fake.state_calls.is_empty());
    assert!(g.items.iter().all(|i| i.evidence.is_err()));
}

// --- forge_cost ------------------------------------------------------------------

#[test]
fn forge_cost_counts_graphql_points_rest_requests_and_304s() {
    let mut fake = Fake {
        budget: Some(budget(4990, 4873)),
        ..Fake::default()
    };
    fake.rows
        .push(row(1, "Blocked by #7", 1, false, &["loom:blocked"]));
    fake.comments.insert(1, Vec::new());
    fake.rows.push(issue(2, "Blocked by #8"));
    fake.states.insert((None, 7), state("OPEN"));
    fake.states.insert((None, 8), state("OPEN"));
    fake.graphql_remaining.push_back(4872);
    let g = gather(&mut fake, Floor::default());
    let m = g.cost.meter;
    assert_eq!((m.graphql_queries, m.graphql_points), (1, 1));
    assert_eq!(m.graphql_remaining, Some(4872));
    // 1 comment page + 2 state reads; the fake answers state reads with 304.
    assert_eq!(m.rest_requests, 3);
    assert_eq!(m.rest_not_modified, 2);
    assert_eq!(
        g.cost.projected,
        Projection {
            graphql: 1,
            core: 5
        }
    );

    let v = serde_json::to_value(&g.cost).unwrap();
    for key in [
        "graphql_queries",
        "graphql_points",
        "graphql_remaining",
        "rest_requests",
        "rest_not_modified",
        "core_remaining",
        "budget_before",
        "projected",
        "floor",
        "budget_refused",
        "budget_stopped",
    ] {
        assert!(v.get(key).is_some(), "forge_cost.{key} missing: {v}");
    }
    assert_eq!(v["budget_before"]["graphql_remaining"], 4873);
    assert_eq!(v["floor"]["core"], 1000);
    assert!(v["budget_refused"].is_null());
}

#[test]
fn the_summary_line_carries_the_same_numbers() {
    let mut cost = ForgeCost::default();
    cost.meter.graphql(Some(3), Some(4870));
    cost.meter.rest(true, Some(4990));
    cost.meter.rest(false, None);
    let line = cost.summary();
    assert!(line.contains("graphql 1 query / 3 point(s) (remaining 4870)"), "{line}");
    assert!(line.contains("REST 2 request(s), 1 not modified (remaining 4990)"), "{line}");
}

// --- pure projection ------------------------------------------------------------

#[test]
fn projection_is_an_upper_bound_per_bucket() {
    let selected = vec![
        (Artifact::Issue, row(1, "", 0, false, &[])),
        (Artifact::Issue, row(2, "", 250, false, &[])),
        (Artifact::Pr, row(3, "", 1, true, &[])),
    ];
    // 2 issues → 1 query; comment pages 0 + 3 + 1, plus 2·3.
    assert_eq!(
        project(&selected),
        Projection {
            graphql: 1,
            core: 10
        }
    );
    let floor = Floor::default();
    assert!(refusal(&budget(1010, 1001), project(&selected), floor).is_none());
    assert!(refusal(&budget(1009, 1001), project(&selected), floor).is_some());
    assert!(refusal(&budget(1010, 1000), project(&selected), floor).is_some());
    // Nothing projected on a bucket, nothing refused on it.
    assert!(refusal(&budget(0, 0), Projection::default(), floor).is_none());
}
