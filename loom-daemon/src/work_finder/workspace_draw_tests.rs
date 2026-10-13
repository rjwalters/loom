use super::*;
use crate::priority_pick::{IMPORTANT_LABEL, VERY_IMPORTANT_LABEL};
use crate::work_finder::WorkItem;

use crate::priority_pick::WeightBonus;

const B: WeightBonus = WeightBonus {
    red_main_fix: crate::priority_pick::DEFAULT_BONUS_RED_MAIN_FIX,
    very_important: crate::priority_pick::DEFAULT_BONUS_VERY_IMPORTANT,
    important: crate::priority_pick::DEFAULT_BONUS_IMPORTANT,
};

/// No bonuses: base weights alone.
const FLAT: WeightBonus = WeightBonus {
    red_main_fix: 0,
    very_important: 0,
    important: 0,
};

fn item(n: u32, created: Option<&str>, labels: &[&str]) -> WorkItem {
    let mut all = vec!["loom:issue".to_string()];
    all.extend(labels.iter().map(|l| (*l).to_string()));
    WorkItem::with_created_at(n, all, created.map(str::to_string))
}

fn cand(idx: usize, n: u32, created: Option<&str>, labels: &[&str]) -> PriorityCandidate {
    super::super::ready_queue::key_of(idx, 0, &item(n, created, labels), false)
}

/// A `loom:main-red-fix` marker-bearing candidate; `repo_red` is whether its
/// repo's `main` is verified red this tick.
fn fix_cand(idx: usize, n: u32, created: Option<&str>, repo_red: bool) -> PriorityCandidate {
    let fix = item(n, created, &[]).with_body(Some("<!-- loom:main-red-fix -->".into()));
    super::super::ready_queue::key_of(idx, 0, &fix, repo_red)
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

/// #11508: the level comes only from `loom:important` /
/// `loom:very-important`. The legacy star labels and a red-main-fix marker
/// carry no level (the bridge is gone).
#[test]
fn the_level_comes_only_from_the_two_priority_labels() {
    let key = |i: &WorkItem| super::super::ready_queue::key_of(0, 0, i, false).level;
    assert_eq!(key(&item(1, None, &["loom:operator-priority"])), Level::Default);
    assert_eq!(key(&item(2, None, &["loom:operator-high-priority"])), Level::Default);
    assert_eq!(key(&item(3, None, &["loom:high-priority-inherited"])), Level::Default);
    let fix = item(4, None, &[]).with_body(Some("<!-- loom:main-red-fix -->".into()));
    assert_eq!(key(&fix), Level::Default, "a red-main fix is labelled, not bridged");
    assert_eq!(key(&item(5, None, &[IMPORTANT_LABEL])), Level::Important);
    let both = item(6, None, &["loom:operator-priority", VERY_IMPORTANT_LABEL]);
    assert_eq!(key(&both), Level::VeryImportant);
}

/// #11742: very-important work adds its bonus to the workspace's weight; it
/// no longer wins outright. Workspace 0 weighs 10, workspace 1 weighs 1 plus
/// the very-important bonus, and both get first draws in proportion.
#[test]
fn very_important_work_adds_its_bonus_to_the_weight() {
    let bonus = WeightBonus {
        very_important: 9,
        ..FLAT
    };
    let mut firsts = std::collections::BTreeMap::new();
    for seed in 0..2000 {
        let candidates = vec![
            cand(0, 1, Some("2026-01-01T00:00:00Z"), &[]),
            cand(1, 7, Some("2026-05-01T00:00:00Z"), &[]),
            cand(1, 8, Some("2026-06-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        ];
        let (order, log) = draw_order(candidates, &[10, 1], seed, &bonus);
        let first = &log.unwrap().steps[0].draw;
        assert_eq!(first.candidates.len(), 2, "no tier filters the pool");
        assert_eq!((first.candidates[1].bonus, first.candidates[1].weight), (9, 10));
        *firsts.entry(numbers(&order)[0]).or_insert(0u32) += 1;
        // Inside workspace 1 the very-important issue still goes first.
        let ws1: Vec<u32> = order
            .iter()
            .filter(|c| c.workspace_idx == 1)
            .map(|c| c.number)
            .collect();
        assert_eq!(ws1, vec![8, 7]);
    }
    // 10 : 10 — about half each.
    let one = firsts.get(&(1, 8)).copied().unwrap_or(0);
    assert!((850..=1150).contains(&one), "{firsts:?}");
}

/// #11742 / #11547: a verified red-main fix adds the red-main bonus while the
/// repo is red, and leads its own workspace's order; on a green repo the
/// marker grants nothing (no bonus, the fix keeps its age place).
#[test]
fn a_red_main_fix_adds_its_bonus_only_while_main_is_red() {
    let build = |red: bool| {
        vec![
            cand(0, 1, Some("2026-01-01T00:00:00Z"), &[]),
            cand(1, 7, Some("2026-05-01T00:00:00Z"), &[]),
            fix_cand(1, 9, Some("2026-09-01T00:00:00Z"), red),
        ]
    };
    let (order, log) = draw_order(build(true), &[10, 1], 1, &B);
    let first = &log.unwrap().steps[0].draw;
    assert_eq!(first.candidates[1].bonus, u64::from(B.red_main_fix));
    let ws1: Vec<u32> = order
        .iter()
        .filter(|c| c.workspace_idx == 1)
        .map(|c| c.number)
        .collect();
    assert_eq!(ws1, vec![9, 7]);

    let (order, log) = draw_order(build(false), &[10, 1], 1, &B);
    assert_eq!(log.unwrap().steps[0].draw.candidates[1].bonus, 0);
    let ws1: Vec<u32> = order
        .iter()
        .filter(|c| c.workspace_idx == 1)
        .map(|c| c.number)
        .collect();
    assert_eq!(ws1, vec![7, 9]);
}

/// The important bonus counts any remaining important candidate, not just
/// the front of the queue, and stops once they are all placed.
#[test]
fn the_important_bonus_follows_the_remaining_queue() {
    let candidates = vec![
        cand(0, 1, None, &[VERY_IMPORTANT_LABEL]),
        cand(0, 2, None, &[IMPORTANT_LABEL]),
        cand(0, 3, None, &[]),
    ];
    let (_, log) = draw_order(candidates, &[1], 1, &B);
    let bonuses: Vec<u64> = log
        .unwrap()
        .steps
        .iter()
        .map(|s| s.draw.candidates[0].bonus)
        .collect();
    let (vi, imp) = (u64::from(B.very_important), u64::from(B.important));
    assert_eq!(bonuses, vec![vi + imp, imp, 0]);
}

/// A workspace of weight 0 is never drawn; its candidates are left out.
#[test]
fn a_weight_zero_workspace_is_left_out_of_the_order() {
    let candidates = vec![
        cand(0, 1, None, &[]),
        cand(1, 2, None, &[VERY_IMPORTANT_LABEL]),
    ];
    let (order, log) = draw_order(candidates, &[1, 0], 1, &B);
    assert_eq!(numbers(&order), vec![(0, 1)]);
    assert_eq!(log.unwrap().draws_total, 1);
}

/// `in_workspace_cmp` and `candidate_cmp` agree on one workspace's
/// candidates, red-main fix included (#11547).
#[test]
fn in_workspace_order_agrees_with_candidate_cmp() {
    let mut cands: Vec<PriorityCandidate> = fixture()
        .iter()
        .map(|i| super::super::ready_queue::key_of(0, 0, i, true))
        .collect();
    cands.push(fix_cand(0, 40, Some("2026-09-01T00:00:00Z"), true));
    cands.push(fix_cand(0, 41, Some("2026-08-01T00:00:00Z"), false));
    for a in &cands {
        for b in &cands {
            assert_eq!(
                in_workspace_cmp(a, b),
                super::super::candidate_cmp(a, b),
                "#{} vs #{}",
                a.number,
                b.number
            );
        }
    }
}

#[test]
fn the_draw_is_weighted_by_base_weight() {
    let mut firsts = std::collections::BTreeMap::new();
    for seed in 0..2000 {
        let candidates = vec![cand(0, 1, None, &[]), cand(1, 2, None, &[])];
        let (order, _) = draw_order(candidates, &[3, 1], seed, &FLAT);
        *firsts.entry(order[0].workspace_idx).or_insert(0u32) += 1;
    }
    // 3 : 1 — 75%, and the lighter workspace still wins sometimes.
    assert!((1400..=1600).contains(&firsts[&0]), "{firsts:?}");
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
    let (a, la) = draw_order(build(), &[5, 5, 1], 42, &B);
    let (b, lb) = draw_order(build(), &[5, 5, 1], 42, &B);
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
    let (order, log) = draw_order(candidates, &[], 1, &B);
    let log = log.unwrap();
    assert_eq!(order.len(), 80);
    assert_eq!(log.draws_total, 80);
    assert_eq!(log.steps.len(), MAX_RECORDED_DRAWS);
}

#[test]
fn nothing_to_draw_is_no_log() {
    let (order, log) = draw_order(Vec::new(), &[1], 1, &B);
    assert!(order.is_empty());
    assert!(log.is_none());
}

#[test]
fn with_seed_pins_the_tick_seed_and_restores_it() {
    assert_eq!(tick_seed(), (TEST_DEFAULT_SEED, None));
    assert_eq!(with_seed(7, tick_seed), (7, None));
    assert_eq!(tick_seed(), (TEST_DEFAULT_SEED, None));
}

/// #11683 known-answer vectors: pin the hash input, the byte slice and the
/// big-endian read (`printf 'loom-draw/v1:%s:%s' m h | shasum -a 256`, first
/// 16 hex digits). The first is quoted in `daemon-reference.md`.
#[test]
fn minute_seed_matches_the_known_answer_vectors() {
    assert_eq!(DRAW_SEED_SCHEME, "loom-draw/v1");
    assert_eq!(minute_seed(29_000_000, "host-a"), 0xfee0_c99c_e201_8983);
    assert_eq!(minute_seed(29_000_000, "host-a"), 18_365_900_956_060_715_395);
    assert_eq!(minute_seed(29_000_001, "host-a"), 0x9c0f_4254_8d82_e7e0);
    assert_eq!(minute_seed(29_000_000, "host-b"), 0xac92_930e_1e14_aaf4);
}

#[test]
fn minute_seed_is_stable_per_minute_and_host_and_differs_across_either() {
    let s = minute_seed(29_000_000, "loom-host-0123456789abcdef");
    assert_eq!(s, minute_seed(29_000_000, "loom-host-0123456789abcdef"));
    assert_ne!(s, minute_seed(29_000_000, "loom-host-fedcba9876543210"));
    assert_ne!(s, minute_seed(29_000_001, "loom-host-0123456789abcdef"));
}

/// Replaying a logged `(minute, host)` reproduces the logged order and draws.
#[test]
fn a_logged_minute_and_host_replay_the_order() {
    let candidates = || -> Vec<PriorityCandidate> {
        (0..4)
            .flat_map(|ws| (1..=5).map(move |n| cand(ws, ws as u32 * 100 + n, None, &[])))
            .collect()
    };
    let weights = [10, 6, 3, 1];
    let (minute, host) = (29_000_000, "host-a");
    let (order, log) = draw_order(candidates(), &weights, minute_seed(minute, host), &B);
    let log = log.unwrap();
    let (replayed, relog) = draw_order(candidates(), &weights, minute_seed(minute, host), &B);
    let relog = relog.unwrap();
    assert_eq!(numbers(&order), numbers(&replayed));
    assert_eq!(log.steps, relog.steps);
    assert_eq!(log.seed, relog.seed);
}

/// `order` attaches nothing when the seed is pinned (no inputs to log).
#[test]
fn a_pinned_tick_logs_no_seed_inputs() {
    let mut report = TickReport::default();
    let ordered = with_seed(7, || order(vec![cand(0, 1, None, &[])], &[1], &mut report));
    assert_eq!(ordered.len(), 1);
    let log = report.workspace_draw.unwrap();
    assert_eq!(log.seed, 7);
    assert_eq!(log.seed_inputs, None);
}

#[test]
fn workspace_keys_round_trip_in_index_order() {
    assert_eq!(workspace_idx(&workspace_key(3)), Some(3));
    assert!(workspace_key(2) < workspace_key(10));
}
