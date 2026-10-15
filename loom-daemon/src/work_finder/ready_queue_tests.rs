//! Coverage for the per-issue ready queue (Issue #8852). Its own file with
//! its own minimal fakes, for the file-size-ratchet reason
//! `prless_retry_tests.rs` gives: `work_finder/tests.rs` is frozen.

#![allow(clippy::unwrap_used)]

use super::super::*;

#[test]
fn short_detail_keeps_one_bounded_line() {
    assert_eq!(ready_queue::short_detail("a\nb"), "a");
    let long = "x".repeat(400);
    assert_eq!(ready_queue::short_detail(&long).chars().count(), 161);
}

// ===================================================================
// candidate_keys — the single dispatch-order seam (Issue #9288)
// ===================================================================

fn oldest_first(a: Option<&String>, b: Option<&String>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// The documented #11103/#11508/#11547 order, written out independently of
/// `candidate_keys`: verified red-main fix first; level highest first;
/// `createdAt` oldest first; number asc. A change to dispatch order must update both this and
/// `candidate_keys` — which is the point.
fn reference_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> std::cmp::Ordering {
    b.main_red_fix
        .cmp(&a.main_red_fix)
        .then(b.level.cmp(&a.level))
        .then(oldest_first(a.created_at.as_ref(), b.created_at.as_ref()))
        .then(a.number.cmp(&b.number))
}

/// Every candidate over a small domain that exercises every key's tie and
/// both directions of every key, plus a workspace priority the keys ignore.
fn candidate_domain() -> Vec<PriorityCandidate> {
    use crate::priority_pick::Level;
    let times = [
        None,
        Some("2026-01-01T00:00:00Z"),
        Some("2026-02-01T00:00:00Z"),
    ];
    let mut out = Vec::new();
    for workspace_weight in [0, 100] {
        for main_red_fix in [false, true] {
            for level in [Level::Default, Level::Important, Level::VeryImportant] {
                for created_at in times {
                    for number in [1, 2] {
                        out.push(PriorityCandidate {
                            workspace_weight,
                            main_red_fix,
                            level,
                            created_at: created_at.map(str::to_string),
                            number,
                            ..PriorityCandidate::default()
                        });
                    }
                }
            }
        }
    }
    out
}

/// The property, exhaustively over every pair in the domain: the
/// lexicographic compare of `candidate_keys` IS `candidate_cmp`, and both
/// match the documented order.
#[test]
fn candidate_keys_lexicographic_compare_is_candidate_cmp() {
    let domain = candidate_domain();
    for a in &domain {
        for b in &domain {
            let by_keys = ready_queue::candidate_keys(a).cmp(&ready_queue::candidate_keys(b));
            assert_eq!(by_keys, candidate_cmp(a, b), "{a:?} vs {b:?}");
            assert_eq!(by_keys, reference_cmp(a, b), "{a:?} vs {b:?}");
        }
    }
}

/// The wire projection names the keys in comparator order, carries each
/// row's own values, and distinguishes every pair the comparator does
/// (workspace priority, not a key, never distinguishes two rows).
#[test]
fn candidate_keys_project_to_named_wire_keys() {
    assert_eq!(
        ready_queue::ordering_names(),
        vec!["main_red_fix", "level", "created_at", "number"]
    );
    let c = PriorityCandidate {
        workspace_weight: 7,
        level: crate::priority_pick::Level::VeryImportant,
        created_at: Some("2026-01-01T00:00:00Z".into()),
        number: 42,
        main_red_fix: true,
        ..PriorityCandidate::default()
    };
    let values: Vec<serde_json::Value> = ready_queue::plan_keys(&c)
        .into_iter()
        .map(|k| k.value)
        .collect();
    assert_eq!(
        values,
        vec![
            serde_json::json!(true),
            serde_json::json!(2),
            serde_json::json!("2026-01-01T00:00:00Z"),
            serde_json::json!(42),
        ]
    );
    let domain = candidate_domain();
    for a in &domain {
        for b in &domain {
            let equal_keys = ready_queue::plan_keys(a) == ready_queue::plan_keys(b);
            assert_eq!(equal_keys, candidate_cmp(a, b).is_eq(), "{a:?} vs {b:?}");
        }
    }
}
