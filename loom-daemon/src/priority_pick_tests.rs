use super::*;

fn key(number: u32, created: Option<&str>, labels: &[&str]) -> IssueKey {
    IssueKey {
        number,
        created_at: created.map(str::to_string),
        level: Level::of(labels),
        main_red_fix: false,
    }
}

fn fix(number: u32, created: Option<&str>) -> IssueKey {
    IssueKey {
        main_red_fix: true,
        ..key(number, created, &[])
    }
}

fn ws(name: &str, priority: u32, vi: bool) -> WorkspaceEntry {
    WorkspaceEntry {
        workspace: name.to_string(),
        priority,
        has_very_important: vi,
        has_red_main_fix: false,
    }
}

fn ws_fix(name: &str, priority: u32) -> WorkspaceEntry {
    WorkspaceEntry {
        has_red_main_fix: true,
        ..ws(name, priority, false)
    }
}

#[test]
fn level_is_the_highest_priority_label() {
    assert_eq!(Level::of::<&str>(&[]), Level::Default);
    assert_eq!(Level::of(&["loom:issue"]), Level::Default);
    assert_eq!(Level::of(&[IMPORTANT_LABEL]), Level::Important);
    assert_eq!(Level::of(&[IMPORTANT_LABEL, VERY_IMPORTANT_LABEL]), Level::VeryImportant);
    assert_eq!(Level::of(&[VERY_IMPORTANT_LABEL, IMPORTANT_LABEL]), Level::VeryImportant);
}

#[test]
fn legacy_labels_carry_no_level() {
    assert_eq!(Level::of(&["loom:operator-priority", "tier:goal-advancing"]), Level::Default);
}

#[test]
fn within_workspace_is_level_then_oldest_then_number() {
    let items = vec![
        key(9, Some("2026-01-01T00:00:00Z"), &[]),
        key(5, Some("2026-03-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        key(4, Some("2026-02-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        key(30, Some("2026-04-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        key(2, Some("2026-02-01T00:00:00Z"), &[IMPORTANT_LABEL]),
        key(1, None, &[]),
    ];
    let order: Vec<u32> = order_in_workspace(items.clone())
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(order, vec![30, 2, 4, 5, 9, 1]);
    assert_eq!(next_in_workspace(&items).unwrap().number, 30);
    assert!(next_in_workspace(&[]).is_none());
}

const T80: u32 = DEFAULT_DRAW_TEMPERATURE;

/// The pre-#11684 `1000 / (1 + p)` assertions are replaced by the softmax's
/// pinned values: the top of the pool weighs the full scale, and the curve
/// falls by `e` every `T` priority units.
#[test]
fn softmax_weights_are_pinned() {
    assert_eq!(softmax_weight(0, 0, T80), DRAW_WEIGHT_SCALE);
    assert_eq!(softmax_weight(12, 12, T80), 1_000_000);
    assert_eq!(softmax_weight(80, 0, 80), 367_879); // e^-1
    assert_eq!(softmax_weight(100, 0, T80), 286_505); // e^-1.25
    assert_eq!(softmax_weight(200, 20, T80), 105_399); // e^-2.25
    assert_eq!(softmax_weight(u32::MAX, 0, T80), 1);
}

#[test]
fn softmax_weight_is_monotone_non_increasing_in_priority() {
    for t in [1, 7, 80, 1000, 100_000] {
        let mut prev = u64::MAX;
        for p in 0..=1000 {
            let w = softmax_weight(p, 0, t);
            assert!(w <= prev, "T={t}: w({p})={w} > w({})={prev}", p - 1);
            prev = w;
        }
    }
}

#[test]
fn softmax_weight_is_never_zero_and_never_overflows() {
    let edges = [0, 1, 12, 1000, u32::MAX - 1, u32::MAX];
    for &p in &edges {
        for &p_min in &edges {
            for t in [0, 1, 80, 100_000, u32::MAX] {
                let w = softmax_weight(p, p_min, t);
                assert!((1..=DRAW_WEIGHT_SCALE).contains(&w), "w({p},{p_min},{t})={w}");
            }
        }
    }
    // A full pool over priorities 0..=1000 sums far inside u64.
    let sum: u64 = (0..=1000).map(|p| softmax_weight(p, 0, 100_000)).sum();
    assert!(sum <= 1001 * DRAW_WEIGHT_SCALE);
}

#[test]
fn small_temperature_concentrates_on_p_min() {
    assert_eq!(softmax_weight(50, 50, 1), DRAW_WEIGHT_SCALE);
    for gap in 20..=1000 {
        assert_eq!(softmax_weight(50 + gap, 50, 1), 1, "gap {gap}");
    }
    let entries = vec![ws("a", 50, false), ws("b", 70, false), ws("c", 300, false)];
    let mut rng = PickRng::from_seed(3);
    let a_wins = (0..10_000)
        .filter(|_| draw_workspace(&entries, 1, &mut rng).unwrap().picked == "a")
        .count();
    assert!(a_wins >= 9_995, "a won {a_wins}/10000 at T=1");
}

#[test]
fn large_temperature_is_near_uniform() {
    let weights: Vec<u64> = (0..=1000).map(|p| softmax_weight(p, 0, 100_000)).collect();
    let max = *weights.iter().max().unwrap() as f64;
    let min = *weights.iter().min().unwrap() as f64;
    assert!(max / min < 1.02, "max/min = {}", max / min);
}

#[test]
fn p_min_is_taken_over_the_tier_filtered_pool() {
    // The very-important pool holds only p=200 workspaces: its top weighs the
    // full scale even though an all-pool entry sits at p=0.
    let entries = vec![
        ws("a/tool", 0, false),
        ws("b/prod", 200, true),
        ws("c/prod", 240, true),
    ];
    let d = draw_workspace(&entries, T80, &mut PickRng::from_seed(1)).unwrap();
    assert_eq!(d.pool, "very-important");
    assert_eq!((d.p_min, d.temperature), (200, T80));
    assert_eq!(d.candidates[0].weight, DRAW_WEIGHT_SCALE);
    assert_eq!(d.candidates[1].weight, softmax_weight(240, 200, T80));
}

/// ACCEPTANCE (#11684): with a fixed seed, N = 10,000 draws land within
/// 1.5 percentage points of the softmax probabilities `w_i / sum(w)`.
#[test]
fn draw_frequencies_match_the_softmax_probabilities() {
    let entries = vec![
        ws("p012", 12, false),
        ws("p066", 66, false),
        ws("p100", 100, false),
        ws("p200", 200, false),
    ];
    let weights: Vec<u64> = entries
        .iter()
        .map(|e| softmax_weight(e.priority, 12, T80))
        .collect();
    let total: u64 = weights.iter().sum();
    let mut rng = PickRng::from_seed(11684);
    let n = 10_000;
    let mut wins = [0usize; 4];
    for _ in 0..n {
        let d = draw_workspace(&entries, T80, &mut rng).unwrap();
        assert_eq!(d.total_weight, total);
        let i = entries
            .iter()
            .position(|e| e.workspace == d.picked)
            .unwrap();
        wins[i] += 1;
    }
    for (i, e) in entries.iter().enumerate() {
        let expected = weights[i] as f64 / total as f64;
        let observed = wins[i] as f64 / f64::from(n);
        assert!(
            (observed - expected).abs() < 0.015,
            "{}: observed {observed:.4}, expected {expected:.4}",
            e.workspace
        );
    }
}

#[test]
fn very_important_workspace_always_wins() {
    let entries = vec![ws("a/tool", 0, false), ws("b/prod", 100, true)];
    for seed in 0..200 {
        let d = draw_workspace(&entries, T80, &mut PickRng::from_seed(seed)).unwrap();
        assert_eq!(d.picked, "b/prod");
        assert_eq!(d.pool, "very-important");
        assert_eq!(d.candidates.len(), 1);
    }
}

#[test]
fn several_very_important_workspaces_are_drawn_among_themselves() {
    let entries = vec![ws("a", 0, true), ws("b", 100, true), ws("c", 0, false)];
    let mut rng = PickRng::from_seed(7);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        let d = draw_workspace(&entries, T80, &mut rng).unwrap();
        assert_ne!(d.picked, "c");
        seen.insert(d.picked);
    }
    assert_eq!(seen.len(), 2);
}

#[test]
fn fixed_seed_reproduces_the_exact_pick_sequence() {
    let entries = vec![
        ws("a", 100, false),
        ws("b", 100, false),
        ws("c", 100, false),
    ];
    let run = |seed| {
        let mut rng = PickRng::from_seed(seed);
        (0..12)
            .map(|_| draw_workspace(&entries, T80, &mut rng).unwrap().picked)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(42), run(42));
    // Pinned sequence: a change here means the draw or the RNG changed
    // (re-pinned for the #11684 softmax weights).
    assert_eq!(run(42).join(""), "aacbacacabcc");
    assert_ne!(run(42), run(43));
}

#[test]
fn draw_is_weighted_and_input_order_independent() {
    let a = vec![ws("a", 0, false), ws("b", 100, false)];
    let b = vec![ws("b", 100, false), ws("a", 0, false)];
    let mut ra = PickRng::from_seed(1);
    let mut rb = PickRng::from_seed(1);
    let mut a_wins = 0;
    for _ in 0..2000 {
        let da = draw_workspace(&a, T80, &mut ra).unwrap();
        assert_eq!(da, draw_workspace(&b, T80, &mut rb).unwrap());
        if da.picked == "a" {
            a_wins += 1;
        }
    }
    // Expected share 1 / (1 + e^-1.25) ~ 77.7% at T = 80.
    assert!((1450..=1650).contains(&a_wins), "a won {a_wins}/2000");
}

#[test]
fn draw_records_candidates_weights_and_pick() {
    let entries = vec![ws("a", 0, false), ws("b", 100, false)];
    let d = draw_workspace(&entries, T80, &mut PickRng::from_seed(5)).unwrap();
    assert_eq!(d.pool, "all");
    assert_eq!(d.total_weight, 1_286_505);
    assert_eq!(d.candidates[0].weight, DRAW_WEIGHT_SCALE);
    assert_eq!((d.temperature, d.p_min), (T80, 0));
    assert!(d.roll < d.total_weight);
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["picked"], d.picked);
    assert!(draw_workspace(&[], T80, &mut PickRng::from_seed(1)).is_none());
}

/// ACCEPTANCE (#11509): with N+2 open very-important issues in a workspace
/// (N = [`VERY_IMPORTANT_CAP`] = 1), exactly the N oldest keep the level, in
/// any input order. The fixtures carry no actor: only number and age.
#[test]
fn only_the_oldest_very_important_issue_holds_the_cap() {
    assert_eq!(VERY_IMPORTANT_CAP, 1);
    let open = vec![
        key(30, Some("2026-03-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        key(10, Some("2026-02-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        key(20, Some("2026-01-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
    ];
    assert_eq!(very_important_holders(&open), vec![20]);
    let mut reversed = open.clone();
    reversed.reverse();
    assert_eq!(very_important_holders(&reversed), vec![20]);
}

#[test]
fn very_important_holder_ties_break_by_number_and_dated_first() {
    let same_age = vec![
        key(9, Some("2026-01-01T00:00:00Z"), &[]),
        key(4, Some("2026-01-01T00:00:00Z"), &[]),
    ];
    assert_eq!(very_important_holders(&same_age), vec![4]);
    let undated = vec![key(1, None, &[]), key(8, Some("2026-05-01T00:00:00Z"), &[])];
    assert_eq!(very_important_holders(&undated), vec![8]);
    // A number listed twice (ready listing + VI listing) counts once, and its
    // known age wins over a missing one.
    let dup = vec![
        key(5, None, &[]),
        key(5, Some("2026-01-01T00:00:00Z"), &[]),
        key(6, Some("2026-02-01T00:00:00Z"), &[]),
    ];
    assert_eq!(very_important_holders(&dup), vec![5]);
    assert!(very_important_holders(&[]).is_empty());
}

#[test]
fn red_main_fix_leads_the_workspace_order() {
    // #11547: a verified red-main fix (default level, newest) goes ahead of
    // every level and every older issue.
    let items = vec![
        key(9, Some("2026-01-01T00:00:00Z"), &[]),
        key(30, Some("2026-02-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
        fix(77, Some("2026-09-01T00:00:00Z")),
        key(2, Some("2026-02-01T00:00:00Z"), &[IMPORTANT_LABEL]),
    ];
    let order: Vec<u32> = order_in_workspace(items.clone())
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(order, vec![77, 30, 2, 9]);
    assert_eq!(next_in_workspace(&items).unwrap().number, 77);
}

#[test]
fn red_main_fix_workspace_wins_the_draw_over_very_important() {
    let entries = vec![
        ws("a/tool", 0, false),
        ws("b/prod", 0, true),
        ws_fix("c/red", 100),
    ];
    for seed in 0..200 {
        let d = draw_workspace(&entries, T80, &mut PickRng::from_seed(seed)).unwrap();
        assert_eq!(d.picked, "c/red");
        assert_eq!(d.pool, "red-main-fix");
        assert_eq!(d.candidates.len(), 1);
    }
}

#[test]
fn without_a_red_main_fix_the_draw_pools_are_unchanged() {
    let vi = vec![ws("a/tool", 0, false), ws("b/prod", 100, true)];
    let all = vec![ws("a/tool", 0, false), ws("b/prod", 100, false)];
    for seed in 0..200 {
        let d = draw_workspace(&vi, T80, &mut PickRng::from_seed(seed)).unwrap();
        assert_eq!((d.pool, d.picked.as_str()), ("very-important", "b/prod"));
        let d = draw_workspace(&all, T80, &mut PickRng::from_seed(seed)).unwrap();
        assert_eq!(d.pool, "all");
        assert_eq!(d.candidates.len(), 2);
    }
}

#[test]
fn red_main_fix_is_not_an_input_to_the_very_important_cap() {
    // The holder is the oldest very-important issue whatever the flag says.
    let open = vec![
        fix(30, Some("2026-03-01T00:00:00Z")),
        key(20, Some("2026-01-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
    ];
    assert_eq!(very_important_holders(&open), vec![20]);
}
