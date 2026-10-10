use super::*;
use crate::priority_pick::{IMPORTANT_LABEL, VERY_IMPORTANT_LABEL};

fn item(n: u32, created: Option<&str>, labels: &[&str]) -> WorkItem {
    let mut all = vec!["loom:issue".to_string()];
    all.extend(labels.iter().map(|l| (*l).to_string()));
    WorkItem::with_created_at(n, all, created.map(str::to_string))
}

fn cand(idx: usize, n: u32, created: Option<&str>, labels: &[&str]) -> PriorityCandidate {
    super::super::ready_queue::key_of(idx, 0, &item(n, created, labels), false)
}

fn numbers(c: &[PriorityCandidate]) -> Vec<(usize, u32)> {
    c.iter().map(|c| (c.workspace_idx, c.number)).collect()
}

/// The fixture the in-workspace tests share: every ordering key exercised.
fn fixture() -> Vec<WorkItem> {
    vec![
        item(9, Some("2026-01-01T00:00:00Z"), &[]),
        item(5, Some("2026-03-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        item(4, Some("2026-02-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        item(30, Some("2026-04-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        item(2, Some("2026-02-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        item(1, None, &[]),
        item(8, Some("2026-01-01T00:00:00Z"), &[]),
    ]
}

#[test]
fn in_workspace_order_is_level_then_oldest_then_number() {
    let mut items = fixture();
    super::super::ready_queue::sort_lanes(&mut items, false);
    let order: Vec<u32> = items.iter().map(|i| i.number).collect();
    // very-important; important by age (2 and 4 tie on age -> number); then
    // the unlabelled by age (8 and 9 tie -> number); the undated last.
    assert_eq!(order, vec![30, 2, 4, 5, 8, 9, 1]);
}

#[test]
fn in_workspace_order_is_independent_of_listing_order() {
    let mut a = fixture();
    let mut b = fixture();
    b.reverse();
    super::super::ready_queue::sort_lanes(&mut a, false);
    super::super::ready_queue::sort_lanes(&mut b, false);
    let na: Vec<u32> = a.iter().map(|i| i.number).collect();
    let nb: Vec<u32> = b.iter().map(|i| i.number).collect();
    assert_eq!(na, nb);
}

#[test]
fn legacy_signals_bridge_onto_the_new_levels() {
    let star = item(1, None, &["loom:operator-priority"]);
    assert_eq!(effective_level(&star, false), Level::Important);
    let high = item(2, None, &["loom:operator-high-priority"]);
    assert_eq!(effective_level(&high, false), Level::VeryImportant);
    let plain = item(3, None, &[]);
    assert_eq!(effective_level(&plain, false), Level::Default);
    assert_eq!(effective_level(&plain, true), Level::VeryImportant, "verified red-main fix");
    let both = item(4, None, &["loom:operator-priority", VERY_IMPORTANT_LABEL]);
    assert_eq!(effective_level(&both, false), Level::VeryImportant, "the higher level wins");
}

#[test]
fn a_workspace_with_very_important_work_always_wins_the_draw() {
    // Workspace 0 weighs 1000 (priority 0) against workspace 1's 9, but only
    // workspace 1 has very-important work: every seed draws workspace 1 first.
    for seed in 0..200 {
        let candidates = vec![
            cand(0, 1, Some("2026-01-01T00:00:00Z"), &[]),
            cand(1, 7, Some("2026-05-01T00:00:00Z"), &[]),
            cand(1, 8, Some("2026-06-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        ];
        let (order, log) = draw_order(candidates, &[0, 100], seed);
        assert_eq!(numbers(&order)[0], (1, 8), "seed {seed}");
        let first = &log.unwrap().steps[0].draw;
        assert_eq!(first.pool, "very-important");
        assert_eq!(first.candidates.len(), 1);
    }
}

#[test]
fn several_very_important_workspaces_are_drawn_among_by_weight() {
    let mut firsts = std::collections::BTreeMap::new();
    for seed in 0..400 {
        let candidates = vec![
            cand(0, 1, None, &[VERY_IMPORTANT_LABEL]),
            cand(1, 2, None, &[VERY_IMPORTANT_LABEL]),
            cand(2, 3, None, &[]),
        ];
        let (order, log) = draw_order(candidates, &[0, 100, 0], seed);
        let log = log.unwrap();
        assert_eq!(log.steps[0].draw.pool, "very-important");
        assert_eq!(log.steps[0].draw.candidates.len(), 2, "only the very-important workspaces");
        // Both very-important issues precede the plain one.
        assert_eq!(order[2].number, 3, "seed {seed}");
        *firsts.entry(order[0].workspace_idx).or_insert(0) += 1;
    }
    // Weight 1000 vs 9: workspace 0 wins far more often, workspace 1 sometimes.
    assert!(firsts[&0] > firsts.get(&1).copied().unwrap_or(0) * 10, "{firsts:?}");
}

#[test]
fn without_very_important_work_the_draw_is_weighted_over_all() {
    let mut firsts = std::collections::BTreeMap::new();
    for seed in 0..2000 {
        let candidates = vec![cand(0, 1, None, &[]), cand(1, 2, None, &[IMPORTANT_LABEL])];
        let (order, log) = draw_order(candidates, &[0, 100], seed);
        assert_eq!(log.unwrap().steps[0].draw.pool, "all");
        *firsts.entry(order[0].workspace_idx).or_insert(0u32) += 1;
    }
    // Weights 1000 : 9 — the low-priority workspace still wins sometimes.
    assert!(firsts[&0] > 1900, "{firsts:?}");
    assert!(firsts.get(&1).copied().unwrap_or(0) > 0, "{firsts:?}");
}

#[test]
fn a_fixed_seed_reproduces_the_exact_order_and_log() {
    let build = || {
        vec![
            cand(0, 1, Some("2026-01-01T00:00:00Z"), &[]),
            cand(0, 2, Some("2026-01-02T00:00:00Z"), &[]),
            cand(1, 10, Some("2026-01-01T00:00:00Z"), &[]),
            cand(1, 11, Some("2026-01-02T00:00:00Z"), &[]),
            cand(2, 20, Some("2026-01-01T00:00:00Z"), &[]),
        ]
    };
    let (a, la) = draw_order(build(), &[10, 10, 50], 42);
    let (b, lb) = draw_order(build(), &[10, 10, 50], 42);
    assert_eq!(numbers(&a), numbers(&b));
    assert_eq!(la, lb);
    let log = la.unwrap();
    assert_eq!(log.seed, 42);
    assert_eq!(log.draws_total, 5);
    // The log replays the order: each step names the issue placed.
    let placed: Vec<(usize, u32)> = log
        .steps
        .iter()
        .map(|s| (s.workspace_idx, s.issue))
        .collect();
    assert_eq!(placed, numbers(&a));
    // Each workspace keeps its own in-workspace order.
    let ws1: Vec<u32> = a
        .iter()
        .filter(|c| c.workspace_idx == 1)
        .map(|c| c.number)
        .collect();
    assert_eq!(ws1, vec![10, 11]);
    // Every draw is over the workspaces that still had candidates.
    assert_eq!(log.steps[0].draw.candidates.len(), 3);
    assert_eq!(log.steps[4].draw.candidates.len(), 1);
}

#[test]
fn the_draw_log_is_capped_but_counts_every_draw() {
    let candidates: Vec<PriorityCandidate> = (1..=80).map(|n| cand(0, n, None, &[])).collect();
    let (order, log) = draw_order(candidates, &[], 1);
    let log = log.unwrap();
    assert_eq!(order.len(), 80);
    assert_eq!(log.draws_total, 80);
    assert_eq!(log.steps.len(), MAX_RECORDED_DRAWS);
}

#[test]
fn nothing_to_draw_is_no_log() {
    let (order, log) = draw_order(Vec::new(), &[0], 1);
    assert!(order.is_empty());
    assert!(log.is_none());
}

#[test]
fn with_seed_pins_the_tick_seed_and_restores_it() {
    assert_eq!(tick_seed(), TEST_DEFAULT_SEED);
    assert_eq!(with_seed(7, tick_seed), 7);
    assert_eq!(tick_seed(), TEST_DEFAULT_SEED);
}

#[test]
fn workspace_keys_round_trip_in_index_order() {
    assert_eq!(workspace_idx(&workspace_key(3)), Some(3));
    assert!(workspace_key(2) < workspace_key(10));
}
