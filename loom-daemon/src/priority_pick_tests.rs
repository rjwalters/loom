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

fn ws(name: &str, weight: u32) -> WorkspaceEntry {
    WorkspaceEntry {
        workspace: name.to_string(),
        weight,
        has_red_main_fix: false,
        has_very_important: false,
        has_important: false,
    }
}

fn ws_vi(name: &str, weight: u32) -> WorkspaceEntry {
    WorkspaceEntry {
        has_very_important: true,
        ..ws(name, weight)
    }
}

fn ws_fix(name: &str, weight: u32) -> WorkspaceEntry {
    WorkspaceEntry {
        has_red_main_fix: true,
        ..ws(name, weight)
    }
}

/// No bonuses: the draw is over base weights alone.
const FLAT: WeightBonus = WeightBonus {
    red_main_fix: 0,
    very_important: 0,
    important: 0,
};

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

#[test]
fn bonus_for_entry_stacks_every_condition_that_holds() {
    let b = WeightBonus::default();
    assert_eq!(b.for_entry(&ws("a", 5)), 0);
    assert_eq!(b.for_entry(&ws_vi("a", 5)), u64::from(DEFAULT_BONUS_VERY_IMPORTANT));
    let all = WorkspaceEntry {
        has_red_main_fix: true,
        has_very_important: true,
        has_important: true,
        ..ws("a", 5)
    };
    assert_eq!(
        b.for_entry(&all),
        u64::from(
            DEFAULT_BONUS_RED_MAIN_FIX + DEFAULT_BONUS_VERY_IMPORTANT + DEFAULT_BONUS_IMPORTANT
        )
    );
}

/// ACCEPTANCE (#11742): weights mean exactly what they say. With N repos at
/// weight 1 and loom at weight N, all holding work, loom's share over a
/// seeded run is about N / (2N - 1).
#[test]
fn draw_share_is_weight_over_total_weight() {
    let n = 10u32;
    let mut entries: Vec<WorkspaceEntry> = (0..n - 1).map(|i| ws(&format!("r{i:02}"), 1)).collect();
    entries.push(ws("loom", n));
    let mut rng = PickRng::from_seed(11742);
    let draws = 20_000;
    let loom_wins = (0..draws)
        .filter(|_| draw_workspace(&entries, &FLAT, &mut rng).unwrap().picked == "loom")
        .count();
    let expected = f64::from(n) / f64::from(2 * n - 1);
    let observed = loom_wins as f64 / f64::from(draws);
    assert!(
        (observed - expected).abs() < 0.015,
        "observed {observed:.4}, expected {expected:.4}"
    );
}

#[test]
fn draw_frequencies_match_effective_weights_with_bonuses() {
    let bonus = WeightBonus {
        red_main_fix: 30,
        very_important: 10,
        important: 5,
    };
    let entries = vec![
        ws("plain", 10),
        ws_vi("vi", 6),
        ws_fix("red", 1),
        WorkspaceEntry {
            has_important: true,
            ..ws("imp", 3)
        },
    ];
    // Effective weights: 10, 16, 31, 8 (total 65).
    let expected = [("imp", 8.0), ("plain", 10.0), ("red", 31.0), ("vi", 16.0)];
    let mut rng = PickRng::from_seed(7);
    let n = 20_000;
    let mut wins = std::collections::BTreeMap::<String, usize>::new();
    for _ in 0..n {
        let d = draw_workspace(&entries, &bonus, &mut rng).unwrap();
        assert_eq!(d.total_weight, 65);
        *wins.entry(d.picked).or_default() += 1;
    }
    for (name, w) in expected {
        let observed = wins.get(name).copied().unwrap_or(0) as f64 / f64::from(n);
        assert!((observed - w / 65.0).abs() < 0.015, "{name}: observed {observed:.4}");
    }
}

#[test]
fn no_tier_wins_outright() {
    // A red-main fix or very-important work makes a workspace likelier, never
    // certain: the plain workspace still gets drawn.
    let entries = vec![ws("a/plain", 10), ws_vi("b/vi", 1), ws_fix("c/red", 1)];
    let mut rng = PickRng::from_seed(3);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2_000 {
        seen.insert(
            draw_workspace(&entries, &WeightBonus::default(), &mut rng)
                .unwrap()
                .picked,
        );
    }
    assert_eq!(seen.len(), 3, "{seen:?}");
}

#[test]
fn weight_zero_is_never_drawn_even_with_bonuses() {
    let entries = vec![ws("a", 1), ws_fix("b", 0)];
    for seed in 0..200 {
        let d = draw_workspace(&entries, &WeightBonus::default(), &mut PickRng::from_seed(seed))
            .unwrap();
        assert_eq!(d.picked, "a");
        assert_eq!(d.candidates.len(), 1);
    }
    assert!(draw_workspace(&[ws("z", 0)], &FLAT, &mut PickRng::from_seed(1)).is_none());
}

#[test]
fn fixed_seed_reproduces_the_exact_pick_sequence() {
    let entries = vec![ws("a", 1), ws("b", 1), ws("c", 1)];
    let run = |seed| {
        let mut rng = PickRng::from_seed(seed);
        (0..12)
            .map(|_| draw_workspace(&entries, &FLAT, &mut rng).unwrap().picked)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(42), run(42));
    assert_ne!(run(42), run(43));
}

#[test]
fn draw_is_input_order_independent() {
    let a = vec![ws("a", 3), ws_vi("b", 1)];
    let b = vec![ws_vi("b", 1), ws("a", 3)];
    let mut ra = PickRng::from_seed(1);
    let mut rb = PickRng::from_seed(1);
    for _ in 0..500 {
        assert_eq!(
            draw_workspace(&a, &WeightBonus::default(), &mut ra),
            draw_workspace(&b, &WeightBonus::default(), &mut rb)
        );
    }
}

#[test]
fn draw_records_base_weights_bonuses_and_pick() {
    let entries = vec![ws("a", 2), ws_vi("b", 1)];
    let d = draw_workspace(&entries, &WeightBonus::default(), &mut PickRng::from_seed(5)).unwrap();
    assert_eq!(d.candidates[0].workspace, "a");
    assert_eq!(
        (d.candidates[0].base_weight, d.candidates[0].bonus, d.candidates[0].weight),
        (2, 0, 2)
    );
    let vi = u64::from(DEFAULT_BONUS_VERY_IMPORTANT);
    assert_eq!(
        (d.candidates[1].base_weight, d.candidates[1].bonus, d.candidates[1].weight),
        (1, vi, 1 + vi)
    );
    assert_eq!(d.total_weight, 3 + vi);
    assert!(d.roll < d.total_weight);
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["picked"], d.picked);
    assert!(draw_workspace(&[], &FLAT, &mut PickRng::from_seed(1)).is_none());
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
fn red_main_fix_is_not_an_input_to_the_very_important_cap() {
    // The holder is the oldest very-important issue whatever the flag says.
    let open = vec![
        fix(30, Some("2026-03-01T00:00:00Z")),
        key(20, Some("2026-01-01T00:00:00Z"), &[VERY_IMPORTANT_LABEL]),
    ];
    assert_eq!(very_important_holders(&open), vec![20]);
}
