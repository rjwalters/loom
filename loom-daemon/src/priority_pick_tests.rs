use super::*;

fn key(number: u32, created: Option<&str>, labels: &[&str]) -> IssueKey {
    IssueKey {
        number,
        created_at: created.map(str::to_string),
        level: Level::of(labels),
    }
}

fn ws(name: &str, priority: u32, vi: bool) -> WorkspaceEntry {
    WorkspaceEntry {
        workspace: name.to_string(),
        priority,
        has_very_important: vi,
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

#[test]
fn weights_fall_with_priority_number() {
    assert_eq!(weight_for_priority(0), 1000);
    assert_eq!(weight_for_priority(10), 90);
    assert_eq!(weight_for_priority(100), 9);
    assert_eq!(weight_for_priority(u32::MAX), 1);
}

#[test]
fn very_important_workspace_always_wins() {
    let entries = vec![ws("a/tool", 0, false), ws("b/prod", 100, true)];
    for seed in 0..200 {
        let d = draw_workspace(&entries, &mut PickRng::from_seed(seed)).unwrap();
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
        let d = draw_workspace(&entries, &mut rng).unwrap();
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
            .map(|_| draw_workspace(&entries, &mut rng).unwrap().picked)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(42), run(42));
    // Pinned sequence: a change here means the draw or the RNG changed.
    assert_eq!(run(42).join(""), "cccacbccbbca");
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
        let da = draw_workspace(&a, &mut ra).unwrap();
        assert_eq!(da, draw_workspace(&b, &mut rb).unwrap());
        if da.picked == "a" {
            a_wins += 1;
        }
    }
    // Expected share 1000/1009 ~ 99%.
    assert!(a_wins > 1900, "a won {a_wins}/2000");
}

#[test]
fn draw_records_candidates_weights_and_pick() {
    let entries = vec![ws("a", 0, false), ws("b", 100, false)];
    let d = draw_workspace(&entries, &mut PickRng::from_seed(5)).unwrap();
    assert_eq!(d.pool, "all");
    assert_eq!(d.total_weight, 1009);
    assert_eq!(d.candidates[0].weight, 1000);
    assert!(d.roll < d.total_weight);
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["picked"], d.picked);
    assert!(draw_workspace(&[], &mut PickRng::from_seed(1)).is_none());
}
