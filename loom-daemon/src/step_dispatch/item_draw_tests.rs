//! Tests for the item draw: weight terms, exclusions, the very-important cap
//! across stages, seed replay and the empirical distribution.

use super::*;
use crate::work_finder::step_item::ItemState;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    t("2026-10-14T12:00:00Z")
}

fn item(issue: Option<u32>, pr: Option<u32>, verdict: Verdict, lane: Lane) -> Item {
    Item {
        issue,
        pr,
        state: ItemState::Ready,
        verdict,
        lane,
        send_back: false,
    }
}

fn step_input(issue: u32, step: Step, labels: &[&str], created: &str) -> DrawInput {
    DrawInput {
        item: item(Some(issue), None, Verdict::Step(step), Lane::Normal),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some(t(created)),
        stage_entry: None,
    }
}

fn ctx() -> WeighContext {
    WeighContext {
        now: now(),
        build_backoff_engaged: false,
        pool_exhausted_roles: BTreeSet::new(),
        workspace_idx: 0,
    }
}

fn key(tick_counter: u64) -> SeedKey {
    SeedKey {
        minute: 29_340_000,
        host: "host-a".into(),
        tick_counter,
        workspace: "acme/widgets".into(),
    }
}

const DAY: u64 = MINUTES_PER_DAY;

#[test]
fn defaults_are_the_ruled_values() {
    let w = ItemWeights::default();
    assert_eq!(
        (w.red_main, w.very_important, w.important, w.age_cap_days, w.age_divisor_days),
        (50, 8, 3, 90, 30)
    );
}

#[test]
fn age_term_is_one_plus_capped_days_over_divisor() {
    let w = ItemWeights::default();
    let at =
        |days: u64| weight_terms(&w, Lane::Normal, Level::Default, days * DAY, AgeSource::Created);
    assert_eq!(at(0).age_permille, 1000);
    assert_eq!(at(30).age_permille, 2000);
    assert_eq!(at(90).age_permille, 4000);
    assert_eq!(at(400).age_permille, 4000, "capped at 90 days");
    // weight = priority × age × divisor_min, exactly.
    assert_eq!(at(30).weight, 2 * 30 * DAY);
    let vi = weight_terms(&w, Lane::Normal, Level::VeryImportant, 30 * DAY, AgeSource::Mirror);
    assert_eq!(vi.priority, 8);
    assert_eq!(vi.weight, 8 * 2 * 30 * DAY);
}

#[test]
fn red_main_lane_takes_the_red_main_factor_over_the_level() {
    let w = ItemWeights::default();
    assert_eq!(w.priority(Lane::RedMain, Level::VeryImportant), 50);
    assert_eq!(w.priority(Lane::Normal, Level::Important), 3);
    assert_eq!(w.priority(Lane::Capability, Level::Default), 1);
}

#[test]
fn stage_entry_wins_over_created_and_none_is_age_one() {
    let mut a = step_input(1, Step::Judge, &[], "2026-01-01T00:00:00Z");
    a.stage_entry = Some(StageEntry {
        at: t("2026-10-04T12:00:00Z"),
        source: Source::Events,
    });
    let mut b = step_input(2, Step::Judge, &[], "2026-10-13T12:00:00Z");
    b.created_at = Some(t("2026-10-13T12:00:00Z"));
    let mut c = step_input(3, Step::Judge, &[], "2026-10-13T12:00:00Z");
    c.created_at = None;
    let out = weigh(&[a, b, c], &ctx(), &ItemWeights::default());
    let terms: Vec<_> = out
        .pool
        .iter()
        .map(|c| (c.terms.age_minutes, c.terms.age_source))
        .collect();
    assert_eq!(
        terms,
        vec![
            (10 * DAY, AgeSource::Events),
            (DAY, AgeSource::Created),
            (0, AgeSource::None),
        ]
    );
    assert_eq!(AgeSource::Events.as_str(), "events");
    assert_eq!(AgeSource::None.as_str(), "none");
}

#[test]
fn passes_backoff_and_exhausted_pools_are_excluded_and_counted() {
    let mut inputs = vec![
        step_input(1, Step::Builder, &[], "2026-10-01T00:00:00Z"),
        step_input(2, Step::Builder, &[], "2026-10-01T00:00:00Z"),
        step_input(3, Step::Judge, &[], "2026-10-01T00:00:00Z"),
        step_input(4, Step::Curator, &[], "2026-10-01T00:00:00Z"),
        step_input(5, Step::CuratorKeepOrClose, &[], "2026-10-01T00:00:00Z"),
    ];
    inputs[1].item.lane = Lane::RedMain;
    for (n, reason) in [
        (6, PassReason::Claimed),
        (7, PassReason::Claimed),
        (8, PassReason::Epic),
    ] {
        inputs.push(DrawInput {
            item: item(Some(n), None, Verdict::Pass(reason), Lane::Normal),
            labels: Vec::new(),
            created_at: None,
            stage_entry: None,
        });
    }
    let mut c = ctx();
    c.build_backoff_engaged = true;
    c.pool_exhausted_roles.insert("curator".into());
    let out = weigh(&inputs, &c, &ItemWeights::default());
    let pooled: Vec<u32> = out.pool.iter().map(|c| c.item.issue.unwrap()).collect();
    assert_eq!(pooled, vec![2, 3], "red-main builder is exempt from the back-off");
    let counts: Vec<(&str, usize)> = out.excluded.iter().map(|(e, n)| (e.as_str(), *n)).collect();
    assert_eq!(
        counts,
        vec![
            ("claimed", 2),
            ("epic", 1),
            ("pool_exhausted", 2),
            ("build_backoff", 1)
        ]
    );
}

#[test]
fn the_vi_cap_demotes_across_stages_before_weighting() {
    // Two very-important items at different stages: the older live one
    // holds the slot, the younger (a Judge step) ranks as important.
    let holder = step_input(
        10,
        Step::Builder,
        &["loom:issue", "loom:very-important"],
        "2026-09-01T00:00:00Z",
    );
    let mut judged = step_input(
        11,
        Step::Judge,
        &["loom:building", "loom:very-important"],
        "2026-09-02T00:00:00Z",
    );
    judged.item.pr = Some(12);
    // A blocked very-important item cannot hold the slot, and is demoted too.
    let blocked = DrawInput {
        item: item(Some(9), None, Verdict::Pass(PassReason::Blocked), Lane::Normal),
        labels: vec!["loom:blocked".into(), "loom:very-important".into()],
        created_at: Some(t("2026-08-01T00:00:00Z")),
        stage_entry: None,
    };
    let out = weigh(&[blocked, holder, judged], &ctx(), &ItemWeights::default());
    let levels: Vec<(u32, Level, u32)> = out
        .pool
        .iter()
        .map(|c| (c.item.issue.unwrap(), c.terms.level, c.terms.priority))
        .collect();
    assert_eq!(levels, vec![(10, Level::VeryImportant, 8), (11, Level::Important, 3)]);
    let demoted: Vec<(u32, u32)> = out.demotions.iter().map(|d| (d.number, d.holder)).collect();
    assert_eq!(demoted, vec![(9, 10), (11, 10)]);
}

#[test]
fn the_seed_is_the_documented_sha256_prefix() {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(b"loom-draw/v2:29340000:host-a:7:acme/widgets:3");
    let expected = u64::from_be_bytes(digest[..8].try_into().unwrap());
    assert_eq!(item_seed(&key(7), 3), expected);
    assert_ne!(item_seed(&key(7), 3), item_seed(&key(8), 3), "tick counter is an input");
    assert_ne!(item_seed(&key(7), 3), item_seed(&key(7), 4), "draw index is an input");
}

/// Stage ages (days) inside and past the 90-day cap, so the age term varies.
const AGES: [i64; 8] = [0, 3, 12, 30, 55, 80, 89, 120];

fn pool_of(n: u32) -> Vec<Candidate> {
    let inputs: Vec<DrawInput> = (1..=n)
        .map(|i| {
            let labels: &[&str] = if i % 3 == 0 { &["loom:important"] } else { &[] };
            let mut input = step_input(i, Step::Judge, labels, "2025-01-01T00:00:00Z");
            input.item.pr = Some(1000 + i);
            input.stage_entry = Some(StageEntry {
                at: now() - chrono::Duration::days(AGES[(i as usize) % AGES.len()]),
                source: Source::Mirror,
            });
            input
        })
        .collect();
    weigh(&inputs, &ctx(), &ItemWeights::default()).pool
}

#[test]
fn the_pool_spans_the_age_term() {
    let ages: BTreeSet<u64> = pool_of(8).iter().map(|c| c.terms.age_permille).collect();
    assert_eq!(
        ages.into_iter().collect::<Vec<_>>(),
        vec![1000, 1100, 1400, 2000, 2833, 3666, 3966, 4000]
    );
}

#[test]
fn replay_from_the_pick_decision_record_gives_the_same_picks() {
    use crate::observability::pick_decision::{item_draw_record, ItemDrawWorkspace};
    use crate::telemetry::kinds::pick_decision::PickItemDraw;
    let k = key(42);
    let mut draw = ItemDraw::new(pool_of(40), k.clone());
    let start = draw.starting_pool().to_vec();
    let draws: Vec<DrawRecord> = draw.by_ref().take(25).collect();
    assert_eq!(draws.len(), 25);
    let excluded = BTreeMap::new();
    let record = item_draw_record(
        k.minute,
        &k.host,
        k.tick_counter,
        &[ItemDrawWorkspace {
            repo: &k.workspace,
            excluded: &excluded,
            pool: &start,
            draws: &draws,
        }],
    )
    .unwrap();
    // Through the wire: only what the record carries.
    let parsed: PickItemDraw =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    let replayed = replay_record(&parsed).unwrap();
    let expected: Vec<(Option<u32>, Option<u32>)> = draws
        .iter()
        .map(|d| (d.picked.item.issue, d.picked.item.pr))
        .collect();
    let got: Vec<(Option<u32>, Option<u32>)> = replayed.iter().map(|p| (p.issue, p.pr)).collect();
    assert_eq!(got, expected);
    // Same inputs, same sequence; another tick counter, another one.
    let again: Vec<DrawRecord> = ItemDraw::new(pool_of(40), k.clone()).take(25).collect();
    assert_eq!(again, draws);
    let other: Vec<DrawRecord> = ItemDraw::new(pool_of(40), key(43)).take(25).collect();
    assert_ne!(other, draws);
    // A tampered record does not replay.
    let mut bad = parsed.clone();
    bad.draws[3].picked_index += 1;
    assert!(replay_record(&bad).is_err());
    let mut bad = parsed;
    bad.pools[0].weight += 1;
    assert!(replay_record(&bad).is_err());
}

#[test]
fn every_item_is_drawn_once() {
    let mut numbers: Vec<u32> = ItemDraw::new(pool_of(12), key(7))
        .map(|d| d.picked.item.issue.unwrap())
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=12).collect::<Vec<_>>());
}

#[test]
fn an_empty_pool_draws_nothing() {
    assert!(ItemDraw::new(Vec::new(), key(1)).next().is_none());
}

#[test]
fn the_roll_is_in_range_and_the_total_never_saturates() {
    for seed in 0..2000u64 {
        for total in [1u64, 2, 3, 7, 1 << 40, u64::MAX / 3 * 2] {
            assert!(roll_for(seed, total) < total);
        }
    }
    assert_eq!(roll_for(5, 1), 0);
    assert_eq!(total_weight([u64::MAX, 1]), Err(DrawError::WeightOverflow));
    assert_eq!(total_weight([2, 3]), Ok(5));
    assert_eq!(pick_by_roll([2, 0, 3], 1), Some(0));
    assert_eq!(pick_by_roll([2, 0, 3], 2), Some(2), "a weight-0 row is never picked");
    assert_eq!(pick_by_roll([2, 3], 5), None);
}

#[test]
fn first_draw_frequencies_match_the_weights() {
    let pool = pool_of(8);
    let total: u64 = pool.iter().map(|c| c.terms.weight).sum();
    let trials = 40_000u64;
    let mut hits = vec![0u64; pool.len()];
    for tick in 0..trials {
        let (i, _, _) = pick_index(&pool, item_seed(&key(tick), 0))
            .unwrap()
            .unwrap();
        hits[i] += 1;
    }
    for (c, h) in pool.iter().zip(&hits) {
        #[allow(clippy::cast_precision_loss)]
        let (expected, seen) = (c.terms.weight as f64 / total as f64, *h as f64 / trials as f64);
        assert!(
            (expected - seen).abs() < 0.01,
            "item {}: expected {expected:.4}, saw {seen:.4}",
            c.item.issue.unwrap()
        );
    }
}

#[test]
fn the_layer_overlays_in_range_values_only() {
    let layer = serde_json::json!({"dispatch": {
        "itemWeightRedMain": 100,
        "itemWeightImportant": 0,
        "itemAgeDivisorDays": 7,
        "itemAgeCapDays": "x"
    }});
    let w = ItemWeights::from_layer(&layer);
    assert_eq!(w.red_main, 100);
    assert_eq!(w.important, DEFAULT_WEIGHT_IMPORTANT, "0 is out of range");
    assert_eq!(w.age_divisor_days, 7);
    assert_eq!(w.age_cap_days, DEFAULT_AGE_CAP_DAYS);
    assert_eq!(ItemWeights::from_layer(&Value::Null), ItemWeights::default());
}
