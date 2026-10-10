//! The workspace draw (#11103) through the real multi-workspace tick:
//! `tick_multi` -> pass 1 -> `workspace_draw::order` -> `shape_queue` ->
//! pass 2 `dispatch()`.

use super::*;
use crate::priority_pick::VERY_IMPORTANT_LABEL;

fn issue(n: u32, created_at: &str, extra: &[&str]) -> WorkItem {
    let mut labels = vec!["loom:issue".to_string()];
    labels.extend(extra.iter().map(|l| (*l).to_string()));
    WorkItem::with_created_at(n, labels, Some(created_at.to_string()))
}

/// Three workspaces: 0 is a tool repo (priority 0, weight 1000), 1 and 2 are
/// product repos (priority 10, weight 90; priority 100, weight 9).
fn fleet() -> Vec<(FakeSource, RecordingDispatcher)> {
    vec![
        (
            FakeSource::once(vec![
                issue(3, "2026-01-03T00:00:00Z", &[]),
                issue(1, "2026-01-01T00:00:00Z", &[]),
                issue(2, "2026-01-02T00:00:00Z", &[]),
            ]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![
                issue(11, "2025-01-01T00:00:00Z", &[]),
                issue(10, "2025-01-01T00:00:00Z", &[]),
            ]),
            RecordingDispatcher::default(),
        ),
        (
            FakeSource::once(vec![issue(20, "2024-01-01T00:00:00Z", &[])]),
            RecordingDispatcher::default(),
        ),
    ]
}

const PRIORITIES: [u32; 3] = [0, 10, 100];

fn dispatch_sequence(report: &TickReport) -> Vec<(usize, u32)> {
    report
        .admissions
        .iter()
        .filter(|a| a.result == "dispatched")
        .map(|a| (a.workspace_idx, a.issue))
        .collect()
}

/// ACCEPTANCE (#11103): with a fixed seed, the real tick reproduces the exact
/// pick sequence, and `report.workspace_draw` records every draw (candidates,
/// weights, roll, pick) that produced it.
#[test]
fn a_fixed_seed_reproduces_the_exact_dispatch_sequence() {
    let run = || {
        crate::work_finder::workspace_draw::with_seed(7, || {
            let mut multi = fleet();
            tick_multi(&mut multi, &PRIORITIES, 10, &[false, false, false])
        })
    };
    let report = run();
    let sequence = dispatch_sequence(&report);
    assert_eq!(
        sequence,
        vec![(0, 1), (0, 2), (0, 3), (1, 10), (1, 11), (2, 20)],
        "the pinned draw sequence for seed 7"
    );
    assert_eq!(dispatch_sequence(&run()), sequence, "same seed, same sequence");
    assert_eq!(report.plan_order, sequence, "the published plan is the draw order");

    let log = report.workspace_draw.expect("the tick records its draws");
    assert_eq!(log.seed, 7);
    assert_eq!(log.draws_total, 6);
    let picks: Vec<(usize, u32)> = log
        .steps
        .iter()
        .map(|s| (s.workspace_idx, s.issue))
        .collect();
    assert_eq!(picks, sequence);
    let first = &log.steps[0].draw;
    let weights: Vec<u64> = first.candidates.iter().map(|c| c.weight).collect();
    assert_eq!(weights, vec![1000, 90, 9]);
    assert_eq!(first.total_weight, 1099);
    assert!(first.roll < first.total_weight);
}

/// A different seed is a different, equally reproducible draw: the
/// lower-priority workspaces are not starved behind a strict tier.
#[test]
fn some_seed_serves_a_lower_priority_workspace_first() {
    let first_pick = |seed: u64| {
        crate::work_finder::workspace_draw::with_seed(seed, || {
            let mut multi = fleet();
            dispatch_sequence(&tick_multi(&mut multi, &PRIORITIES, 1, &[false, false, false]))[0]
        })
    };
    let firsts: std::collections::BTreeSet<usize> = (0..300).map(|s| first_pick(s).0).collect();
    assert!(firsts.contains(&0));
    assert!(firsts.contains(&1), "weight 90 of 1099 wins some draws: {firsts:?}");
}

/// ACCEPTANCE (#11103): a workspace with dispatchable `loom:very-important`
/// work always wins the draw, however low its priority.
#[test]
fn very_important_work_wins_the_single_slot_for_every_seed() {
    for seed in 0..100 {
        let report = crate::work_finder::workspace_draw::with_seed(seed, || {
            let mut multi = fleet();
            multi[2].0 = FakeSource::once(vec![
                issue(20, "2024-01-01T00:00:00Z", &[]),
                issue(21, "2026-09-01T00:00:00Z", &[VERY_IMPORTANT_LABEL]),
            ]);
            tick_multi(&mut multi, &PRIORITIES, 1, &[false, false, false])
        });
        assert_eq!(dispatch_sequence(&report), vec![(2, 21)], "seed {seed}");
        let log = report.workspace_draw.unwrap();
        assert_eq!(log.steps[0].draw.pool, "very-important");
    }
}

/// An idle multi-workspace tick draws nothing and records no draw.
#[test]
fn an_idle_tick_records_no_draw() {
    let mut multi = vec![(FakeSource::once(Vec::new()), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[0], 4, &[false]);
    assert!(report.workspace_draw.is_none());
}
