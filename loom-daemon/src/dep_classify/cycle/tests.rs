//! Tests for dependency-cycle detection (epic #7810, PR 3).

use super::*;

/// Build a fetcher over a fixed graph. Each entry is `(node, state, refs)`.
fn graph(entries: &[(&str, &str, &[&str])]) -> impl FnMut(&str) -> Option<Node> {
    let map: HashMap<String, Node> = entries
        .iter()
        .map(|(n, state, refs)| {
            let body = refs
                .iter()
                .map(|r| format!("Blocked by {r}"))
                .collect::<Vec<_>>()
                .join("\n");
            (
                (*n).to_string(),
                Node {
                    state: (*state).to_string(),
                    body,
                },
            )
        })
        .collect();
    move |n: &str| map.get(n).cloned()
}

#[test]
fn a_chain_with_no_loop_reports_no_cycle() {
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2"]),
            ("o/r#2", "OPEN", &["o/r#3"]),
            ("o/r#3", "OPEN", &[]),
        ]),
        Budgets::default(),
    );
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(w.is_complete(), "a fully-walked chain must report complete");
}

#[test]
fn a_direct_loop_is_found() {
    let mut w = Walk::new(
        graph(&[("o/r#1", "OPEN", &["o/r#2"]), ("o/r#2", "OPEN", &["o/r#1"])]),
        Budgets::default(),
    );
    match w.run("o/r#1") {
        Outcome::Cycle(c) => {
            assert_eq!(c.first(), c.last(), "the loop must be closed: {c:?}");
            assert!(c.contains(&"o/r#2".to_string()), "{c:?}");
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn a_longer_loop_is_found_and_reported_from_its_first_recurrence() {
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2"]),
            ("o/r#2", "OPEN", &["o/r#3"]),
            ("o/r#3", "OPEN", &["o/r#2"]),
        ]),
        Budgets::default(),
    );
    match w.run("o/r#1") {
        // #1 is not part of the loop, so the segment starts at #2.
        Outcome::Cycle(c) => assert_eq!(
            c,
            vec![
                "o/r#2".to_string(),
                "o/r#3".to_string(),
                "o/r#2".to_string()
            ],
            "the segment must start at the first recurrence, not the root"
        ),
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn a_self_reference_is_not_a_cycle() {
    // The shell skips `ref == node` explicitly. An issue citing itself is a
    // typo, not an unresolvable wait.
    let mut w = Walk::new(graph(&[("o/r#1", "OPEN", &["o/r#1"])]), Budgets::default());
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
}

#[test]
fn a_closed_dependency_is_not_descended_into() {
    // #2 is CLOSED, so the loop through it cannot hold anything shut.
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2"]),
            ("o/r#2", "CLOSED", &["o/r#1"]),
        ]),
        Budgets::default(),
    );
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
}

#[test]
fn an_unreadable_node_is_recorded_and_marks_the_walk_incomplete() {
    // `o/r#404` is REFERENCED but absent from the graph, so the fetch misses.
    // The reference must be numeric: `parse_dependency_refs` requires `#[0-9]+`,
    // so a non-numeric placeholder never becomes a reference at all and the walk
    // would complete with nothing to fetch — which is what a first draft of this
    // test actually asserted.
    let mut w = Walk::new(graph(&[("o/r#1", "OPEN", &["o/r#404"])]), Budgets::default());
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(
        !w.is_complete(),
        "an unreadable node means 'no cycle FOUND', not 'no cycle exists'"
    );
    assert_eq!(w.unreadable(), ["o/r#404"]);
}

#[test]
fn the_depth_budget_truncates_and_is_recorded() {
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2"]),
            ("o/r#2", "OPEN", &["o/r#3"]),
            ("o/r#3", "OPEN", &["o/r#4"]),
            ("o/r#4", "OPEN", &[]),
        ]),
        Budgets {
            max_depth: 2,
            ..Budgets::default()
        },
    );
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(!w.is_complete());
    assert!(w.truncations().contains(&Truncation::Depth), "{:?}", w.truncations());
}

#[test]
fn the_node_budget_truncates_and_is_recorded() {
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2", "o/r#3", "o/r#4"]),
            ("o/r#2", "OPEN", &[]),
            ("o/r#3", "OPEN", &[]),
            ("o/r#4", "OPEN", &[]),
        ]),
        Budgets {
            max_nodes: 2,
            ..Budgets::default()
        },
    );
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(!w.is_complete());
    assert!(w.truncations().contains(&Truncation::Nodes), "{:?}", w.truncations());
}

#[test]
fn the_step_budget_truncates_and_is_recorded() {
    let mut w = Walk::new(
        graph(&[
            ("o/r#1", "OPEN", &["o/r#2", "o/r#3"]),
            ("o/r#2", "OPEN", &[]),
            ("o/r#3", "OPEN", &[]),
        ]),
        Budgets {
            max_steps: 1,
            ..Budgets::default()
        },
    );
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(!w.is_complete());
    assert!(w.truncations().contains(&Truncation::Steps), "{:?}", w.truncations());
}

/// THE invariant. A node whose subtree was truncated must NOT be memoised as
/// explored, because a later path may reach it with budget to spare — and
/// skipping it there would miss a real cycle.
///
/// `#2` is reachable twice: once at depth 2 (where the depth budget truncates
/// beneath it) and once directly from the root at depth 1, where the loop
/// `#2 -> #5 -> #2` is inside budget. Memoising on return marks `#2` explored
/// during the first visit and the second never looks — reporting NO CYCLE for a
/// graph that plainly has one.
#[test]
fn a_truncated_subtree_is_not_memoised() {
    let mut w = Walk::new(
        graph(&[
            // First branch reaches #2 deep, where depth runs out beneath it.
            ("o/r#1", "OPEN", &["o/r#9", "o/r#2"]),
            ("o/r#9", "OPEN", &["o/r#2"]),
            // The loop, reachable within budget from the root's second ref.
            ("o/r#2", "OPEN", &["o/r#5"]),
            ("o/r#5", "OPEN", &["o/r#2"]),
        ]),
        Budgets {
            max_depth: 3,
            ..Budgets::default()
        },
    );
    let outcome = w.run("o/r#1");
    assert!(
        matches!(outcome, Outcome::Cycle(_)),
        "the #2 -> #5 -> #2 loop must still be found via the shallower path; \
         memoising the truncated deep visit would hide it. got {outcome:?}"
    );
}

#[test]
fn a_completed_subtree_is_memoised_so_shared_nodes_cost_one_walk() {
    // The other half: memoisation must still happen when it is safe, or a
    // diamond re-walks its shared tail.
    let mut calls = std::cell::RefCell::new(Vec::<String>::new());
    {
        let inner = graph(&[
            ("o/r#1", "OPEN", &["o/r#2", "o/r#3"]),
            ("o/r#2", "OPEN", &["o/r#4"]),
            ("o/r#3", "OPEN", &["o/r#4"]),
            ("o/r#4", "OPEN", &[]),
        ]);
        let mut inner = inner;
        let mut w = Walk::new(
            |n: &str| {
                calls.borrow_mut().push(n.to_string());
                inner(n)
            },
            Budgets::default(),
        );
        assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
        assert!(w.is_complete());
    }
    let fetched = calls.get_mut();
    let four = fetched.iter().filter(|n| *n == "o/r#4").count();
    assert_eq!(four, 1, "the shared tail must be fetched once, got {fetched:?}");
}

#[test]
fn an_unreachable_root_yields_no_cycle_and_marks_incomplete() {
    let mut w = Walk::new(graph(&[]), Budgets::default());
    assert_eq!(w.run("o/r#1"), Outcome::NoCycle);
    assert!(!w.is_complete());
}

#[test]
fn cycle_segment_starts_at_the_first_recurrence_and_closes_the_loop() {
    let path: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_string()).collect();
    assert_eq!(cycle_segment("b", &path), vec!["b", "c", "b"]);
    assert_eq!(cycle_segment("a", &path), vec!["a", "b", "c", "a"]);
}
