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
        cooldowns: Default::default(),
    }
}

fn key(tick_counter: u64) -> SeedKey {
    SeedKey {
        minute: 29_340_000,
        host: "host-a".into(),
        tick_counter,
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

/// #11900: the ledger's query zeroes `actionable` for a cooling
/// `(item, step)` only, at the tick's clock; the draw keys PR steps on the PR.
#[test]
fn a_cooling_item_step_is_excluded_and_counted() {
    let pr_judge = DrawInput {
        item: item(Some(3), Some(40), Verdict::Step(Step::Judge), Lane::Normal),
        ..step_input(3, Step::Judge, &[], "2026-10-01T00:00:00Z")
    };
    let inputs = vec![
        step_input(1, Step::Builder, &["loom:issue"], "2026-10-01T00:00:00Z"),
        step_input(2, Step::ChampionReevaluate, &["loom:curated"], "2026-10-01T00:00:00Z"),
        pr_judge,
    ];
    let mut c = ctx();
    c.cooldowns
        .insert(1, StepRole::Builder, now() + chrono::Duration::minutes(5));
    c.cooldowns
        .insert(1, StepRole::Curator, now() + chrono::Duration::hours(5));
    c.cooldowns
        .insert(2, StepRole::ChampionPromote, now() + chrono::Duration::hours(6));
    c.cooldowns.insert(40, StepRole::Judge, now());
    let w = weigh(&inputs, &c, &ItemWeights::default());
    assert_eq!(w.excluded.get(&Exclusion::CoolingDown), Some(&2));
    let pooled: Vec<u32> = w.pool.iter().map(Candidate::target).collect();
    assert_eq!(pooled, vec![40], "a window ending now has lapsed");
    assert_eq!(Exclusion::CoolingDown.as_str(), "cooling_down");
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
    let digest = Sha256::digest(b"loom-draw/v3:29340000:host-a:7:3");
    let expected = u64::from_be_bytes(digest[..8].try_into().unwrap());
    assert_eq!(item_seed(&key(7), 3), expected);
    assert_eq!(ITEM_DRAW_SEED_SCHEME, "loom-draw/v3");
    assert_ne!(item_seed(&key(7), 3), item_seed(&key(8), 3), "tick counter is an input");
    assert_ne!(item_seed(&key(7), 3), item_seed(&key(7), 4), "draw index is an input");
}

/// Stage ages (days) inside and past the 90-day cap, so the age term varies.
const AGES: [i64; 8] = [0, 3, 12, 30, 55, 80, 89, 120];

/// `n` Judge steps of workspace `idx`, priorities and ages varying.
fn pool_in(idx: usize, n: u32) -> Vec<Candidate> {
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
    let ctx = WeighContext {
        workspace_idx: idx,
        ..ctx()
    };
    weigh(&inputs, &ctx, &ItemWeights::default()).pool
}

fn pool_of(n: u32) -> Vec<Candidate> {
    pool_in(0, n)
}

fn repo(weight: u32, in_flight: usize) -> RepoTerms {
    RepoTerms { weight, in_flight }
}

fn repos_of(terms: &[(usize, u32, usize)]) -> BTreeMap<usize, RepoTerms> {
    terms
        .iter()
        .map(|&(idx, weight, in_flight)| (idx, repo(weight, in_flight)))
        .collect()
}

fn rows_of(pool: &[Candidate]) -> Vec<PoolRow> {
    pool.iter()
        .map(|c| PoolRow {
            repo: c.workspace_idx,
            item_weight: c.terms.weight,
        })
        .collect()
}

/// The probability each pooled item is drawn first.
fn probabilities(
    pool: &[Candidate],
    repos: &BTreeMap<usize, RepoTerms>,
    knobs: &PoolKnobs,
) -> Vec<f64> {
    let weighed = pool_weights(&rows_of(pool), repos, knobs);
    let total: u64 = weighed.weights.iter().sum();
    weighed
        .weights
        .iter()
        .map(|&w| w as f64 / total as f64)
        .collect()
}

/// Repos of very different backlogs: A (40 items, w 3), B (2 items, w 1) and
/// C (7 items, w 2), the items' priorities and ages mixed.
fn three_repos() -> Vec<Candidate> {
    let mut pool = pool_in(0, 40);
    pool.extend(pool_in(1, 2));
    pool.extend(pool_in(2, 7));
    pool
}

fn knobs(backlog_exponent: f64, busy_exponent: f64) -> PoolKnobs {
    PoolKnobs {
        backlog_exponent,
        busy_exponent,
    }
}

/// The sum of the item weights of workspace `idx`'s items.
fn s_r(pool: &[Candidate], idx: usize) -> f64 {
    pool.iter()
        .filter(|c| c.workspace_idx == idx)
        .map(|c| c.terms.weight as f64)
        .sum()
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() <= 1e-9 * b.max(a).max(1e-12), "{a} != {b}");
}

/// #12185 acceptance: with α = 0 and β = 0 each item's probability is the
/// earlier two-stage draw's: its repo's `w_repo / Σw_repo`, then `iw / S_r`.
#[test]
fn alpha_zero_beta_zero_is_the_two_stage_draw() {
    let pool = three_repos();
    let repos = repos_of(&[(0, 3, 5), (1, 1, 0), (2, 2, 9)]);
    let p = probabilities(&pool, &repos, &knobs(0.0, 0.0));
    for (c, p) in pool.iter().zip(&p) {
        let w = f64::from(repos[&c.workspace_idx].weight);
        close(*p, w / 6.0 * c.terms.weight as f64 / s_r(&pool, c.workspace_idx));
    }
    // A repo's share is its roster share whatever its backlog or in-flight.
    let share = |idx: usize| -> f64 {
        pool.iter()
            .zip(&p)
            .filter(|(c, _)| c.workspace_idx == idx)
            .map(|(_, p)| p)
            .sum()
    };
    close(share(0), 0.5);
    close(share(1), 1.0 / 6.0);
    close(share(2), 1.0 / 3.0);
}

/// α = 1 is a flat pool: probability ∝ `w_repo × iw`, so a repo's share
/// grows with its backlog.
#[test]
fn alpha_one_is_a_flat_pool() {
    let pool = three_repos();
    let repos = repos_of(&[(0, 3, 0), (1, 1, 0), (2, 2, 0)]);
    let p = probabilities(&pool, &repos, &knobs(1.0, 0.0));
    let raw: Vec<f64> = pool
        .iter()
        .map(|c| f64::from(repos[&c.workspace_idx].weight) * c.terms.weight as f64)
        .collect();
    let total: f64 = raw.iter().sum();
    for (r, p) in raw.iter().zip(&p) {
        close(*p, r / total);
    }
    // Equal `w_repo` and equal items: probability is the same for every item,
    // so a repo's share is its item count over the pool's.
    let flat = [pool_in(0, 1), pool_in(1, 1), pool_in(1, 1), pool_in(1, 1)].concat();
    let p = probabilities(&flat, &repos_of(&[(0, 1, 0), (1, 1, 0)]), &knobs(1.0, 0.0));
    close(p[0], p[1] * (flat[0].terms.weight as f64 / flat[1].terms.weight as f64));
}

#[test]
fn alpha_half_scales_by_the_square_root_of_the_backlog() {
    let pool = three_repos();
    let repos = repos_of(&[(0, 3, 0), (1, 1, 0), (2, 2, 0)]);
    let p = probabilities(&pool, &repos, &knobs(0.5, 0.0));
    let raw: Vec<f64> = pool
        .iter()
        .map(|c| {
            f64::from(repos[&c.workspace_idx].weight) * c.terms.weight as f64
                / s_r(&pool, c.workspace_idx).sqrt()
        })
        .collect();
    let total: f64 = raw.iter().sum();
    for (r, p) in raw.iter().zip(&p) {
        close(*p, r / total);
    }
}

/// β > 0: a repo's share falls as its in-flight count rises, by
/// `(1 + in_flight)^(−β)`, and the other repos' shares rise.
#[test]
fn busy_repos_are_drawn_less() {
    let pool = three_repos();
    let share = |in_flight: usize, busy: f64, idx: usize| -> f64 {
        let repos = repos_of(&[(0, 3, in_flight), (1, 1, 0), (2, 2, 0)]);
        let p = probabilities(&pool, &repos, &knobs(0.0, busy));
        pool.iter()
            .zip(&p)
            .filter(|(c, _)| c.workspace_idx == idx)
            .map(|(_, p)| p)
            .sum()
    };
    close(share(0, 1.0, 0), 0.5);
    let (a1, a3) = (share(1, 1.0, 0), share(3, 1.0, 0));
    assert!(a1 < 0.5 && a3 < a1, "{a1} {a3}");
    // β = 0 ignores the count; β = 2 bites harder than β = 1.
    close(share(7, 0.0, 0), 0.5);
    assert!(share(3, 2.0, 0) < a3);
    // A's raw weight is divided by (1 + f)^β: 3 / 4 against B and C's 1 + 2.
    close(share(3, 1.0, 0), (3.0 / 4.0) / (3.0 / 4.0 + 1.0 + 2.0));
    assert!(share(3, 1.0, 1) > 1.0 / 6.0, "the others gain");
}

#[test]
fn a_zero_weight_or_unknown_repo_is_never_drawn_and_a_tiny_item_still_can_be() {
    let pool = three_repos();
    let repos = repos_of(&[(0, 0, 0), (1, 1, 0)]);
    let weighed = pool_weights(&rows_of(&pool), &repos, &knobs(0.3, 0.0));
    for (c, w) in pool.iter().zip(&weighed.weights) {
        assert_eq!(*w == 0, c.workspace_idx != 1, "only repo 1 is drawable");
    }
    // All weights zero: nothing to draw.
    let none = pool_weights(&rows_of(&pool), &repos_of(&[]), &knobs(0.0, 0.0));
    assert!(none.weights.iter().all(|&w| w == 0));
    assert!(ItemDraw::new(pool.clone(), repos_of(&[(0, 0, 0)]), knobs(0.0, 0.0), key(1))
        .next()
        .is_none());
    // Rounding to integers never turns a positive weight into 0.
    let rows = [
        PoolRow {
            repo: 0,
            item_weight: 1,
        },
        PoolRow {
            repo: 1,
            item_weight: 1_000_000_000_000_000,
        },
    ];
    let w = pool_weights(&rows, &repos_of(&[(0, 1, 0), (1, 1, 0)]), &knobs(1.0, 0.0));
    assert!(w.weights[0] >= 1, "{:?}", w.weights);
}

#[test]
fn the_exponents_come_from_the_layer_in_range_only() {
    let layer = serde_json::json!({"dispatch": {"backlogExponent": 0.4, "busyExponent": 2}});
    assert_eq!(PoolKnobs::from_layer(&layer), knobs(0.4, 2.0));
    let layer = serde_json::json!({"dispatch": {"backlogExponent": 1.5, "busyExponent": -1}});
    assert_eq!(PoolKnobs::from_layer(&layer), PoolKnobs::default());
    assert_eq!(PoolKnobs::from_layer(&Value::Null), knobs(0.0, 0.0));
}

#[test]
fn the_pool_spans_the_age_term() {
    let ages: BTreeSet<u64> = pool_of(8).iter().map(|c| c.terms.age_permille).collect();
    assert_eq!(
        ages.into_iter().collect::<Vec<_>>(),
        vec![1000, 1100, 1400, 2000, 2833, 3666, 3966, 4000]
    );
}

fn draw_over(pool: Vec<Candidate>, k: SeedKey) -> ItemDraw {
    ItemDraw::new(pool, repos_of(&[(0, 3, 4), (1, 1, 0), (2, 2, 1)]), knobs(0.6, 0.5), k)
}

/// The record a tick would log, through the wire.
fn wire_record(
    draw: &ItemDraw,
    draws: &[DrawRecord],
    k: &SeedKey,
) -> crate::telemetry::kinds::pick_decision::PickItemDraw {
    use crate::observability::pick_decision::{item_draw_record, ItemDrawPool, ItemDrawWorkspace};
    let excluded = BTreeMap::new();
    let names = ["acme/widgets", "acme/gadgets", "acme/gizmos"];
    let workspaces: Vec<ItemDrawWorkspace<'_>> = names
        .iter()
        .enumerate()
        .map(|(idx, repo)| ItemDrawWorkspace {
            idx,
            repo,
            excluded: &excluded,
        })
        .collect();
    let record = item_draw_record(
        k.minute,
        &k.host,
        k.tick_counter,
        &workspaces,
        &ItemDrawPool {
            draw,
            draws,
            overflow: false,
        },
    )
    .unwrap();
    serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap()
}

#[test]
fn replay_from_the_pick_decision_record_gives_the_same_picks() {
    let k = key(42);
    let mut draw = draw_over(three_repos(), k.clone());
    let draws: Vec<DrawRecord> = draw.by_ref().take(25).collect();
    assert_eq!(draws.len(), 25);
    let parsed = wire_record(&draw, &draws, &k);
    assert_eq!(parsed.seed_scheme, "loom-draw/v3");
    assert_eq!(
        (parsed.backlog_exponent.as_str(), parsed.busy_exponent.as_str()),
        ("0.6", "0.5")
    );
    let repo_a = &parsed.repos[0];
    assert_eq!(
        (repo_a.repo.as_str(), repo_a.weight, repo_a.in_flight, repo_a.items),
        ("acme/widgets", 3, 4, 40)
    );
    // The draws carry the repo terms of the drawn item.
    let first = &parsed.draws[0];
    assert_eq!(first.draw_weight, draws[0].terms.weight);
    assert_eq!(first.repo_items, draws[0].terms.repo.items);
    assert_eq!(first.in_flight_repo, draws[0].terms.repo.in_flight);
    assert!(first.draw_weight <= first.total_weight);

    let replayed = replay_record(&parsed).unwrap();
    let expected: Vec<(usize, Option<u32>, Option<u32>)> = draws
        .iter()
        .map(|d| (d.picked.workspace_idx, d.picked.item.issue, d.picked.item.pr))
        .collect();
    let names = ["acme/widgets", "acme/gadgets", "acme/gizmos"];
    let got: Vec<(usize, Option<u32>, Option<u32>)> = replayed
        .iter()
        .map(|p| (names.iter().position(|n| *n == p.repo).unwrap(), p.issue, p.pr))
        .collect();
    assert_eq!(got, expected);
    // Same inputs, same sequence; another tick counter, another one.
    let again: Vec<DrawRecord> = draw_over(three_repos(), k.clone()).take(25).collect();
    assert_eq!(again, draws);
    let other: Vec<DrawRecord> = draw_over(three_repos(), key(43)).take(25).collect();
    assert_ne!(other, draws);
    // A tampered record does not replay.
    let mut bad = parsed.clone();
    bad.draws[3].picked_index += 1;
    assert!(replay_record(&bad).is_err());
    let mut bad = parsed.clone();
    bad.pools[0].weight += 1;
    assert!(replay_record(&bad).is_err());
    let mut bad = parsed.clone();
    bad.repos[0].in_flight += 1;
    assert!(replay_record(&bad).is_err(), "the in-flight count is an input");
    let mut bad = parsed;
    bad.busy_exponent = "0.6".into();
    assert!(replay_record(&bad).is_err(), "so is the busy-ness exponent");
}

/// A repo the per-repo cap closes mid-tick leaves the pool for the draws
/// after it, and the record says when, so the sequence still replays; the
/// record goes through chunks and is reassembled from the wire (#11909).
#[test]
fn a_chunked_record_with_a_closed_repo_replays() {
    use crate::telemetry::kinds::pick_decision::{
        split_into_chunks, PickDecisionRecord, PickItemDraw, PickTick,
    };
    let k = key(9);
    let mut draw = draw_over(three_repos(), k.clone());
    let mut draws = Vec::new();
    for i in 0..14 {
        if i == 6 {
            draw.close_repo(0);
        }
        draws.push(draw.try_next().unwrap().unwrap());
    }
    assert!(draws[6..].iter().all(|d| d.picked.workspace_idx != 0));
    assert_eq!(draw.closures(), &[(0, 6)]);
    let record = wire_record(&draw, &draws, &k);
    assert_eq!(record.closures.len(), 1);
    assert_eq!(
        (record.closures[0].repo.as_str(), record.closures[0].after_draw),
        ("acme/widgets", 6)
    );

    let tick = PickTick {
        role: "work_finder".into(),
        host: "host-a".into(),
        tick_id: "work_finder-1".into(),
        started_at: now(),
        ended_at: now(),
        outcome: "dispatched".into(),
    };
    let whole = PickDecisionRecord::build(tick, Vec::new()).with_item_draw(Some(record));
    let chunks = split_into_chunks(whole, 4_000);
    assert!(chunks.len() > 2, "{} chunks", chunks.len());
    let mut parsed: Vec<PickItemDraw> = chunks
        .iter()
        .map(|c| {
            let text = serde_json::to_string(c).unwrap();
            serde_json::from_str::<PickDecisionRecord>(&text)
                .unwrap()
                .item_draw
                .unwrap()
        })
        .collect();
    // The header is on every chunk, so it is read from one.
    assert!(parsed
        .iter()
        .all(|c| c.repos.len() == 3 && c.closures.len() == 1));
    let mut joined = parsed.remove(0);
    for chunk in parsed {
        joined.pools.extend(chunk.pools);
        joined.draws.extend(chunk.draws);
    }
    let replayed = replay_record(&joined).unwrap();
    let expected: Vec<Option<u32>> = draws.iter().map(|d| d.picked.item.issue).collect();
    assert_eq!(replayed.iter().map(|p| p.issue).collect::<Vec<_>>(), expected);

    // A dropped draw breaks the consecutive indices; a wrong pool size, a
    // forgotten closure and a gap in the logged pool are caught too.
    let mut gap = joined.clone();
    gap.draws.remove(0);
    assert!(replay_record(&gap).unwrap_err().contains("skips draw 0"));
    let mut size = joined.clone();
    size.draws[1].pool_size += 1;
    assert!(replay_record(&size).unwrap_err().contains("pool_size"));
    let mut open = joined.clone();
    open.closures.clear();
    assert!(replay_record(&open).is_err());
    let mut hole = joined;
    hole.pools.retain(|p| p.index != 4);
    assert!(replay_record(&hole).unwrap_err().contains("not indexed"));
}

#[test]
fn every_item_is_drawn_once() {
    let mut numbers: Vec<u32> = draw_over(pool_of(12), key(7))
        .map(|d| d.picked.item.issue.unwrap())
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=12).collect::<Vec<_>>());
}

#[test]
fn an_empty_pool_draws_nothing() {
    assert!(ItemDraw::new(Vec::new(), BTreeMap::new(), PoolKnobs::default(), key(1))
        .next()
        .is_none());
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

/// The seeded draw's first-pick frequencies match the computed probabilities.
#[test]
fn first_draw_frequencies_match_the_weights() {
    let pool = three_repos();
    let repos = repos_of(&[(0, 3, 2), (1, 1, 0), (2, 2, 0)]);
    let k = knobs(0.5, 1.0);
    let expected = probabilities(&pool, &repos, &k);
    let trials = 60_000u64;
    let mut hits = vec![0u64; pool.len()];
    for tick in 0..trials {
        let mut draw = ItemDraw::new(pool.clone(), repos.clone(), k, key(tick));
        let first = draw.try_next().unwrap().unwrap();
        hits[first.picked_index] += 1;
    }
    for ((c, h), expected) in pool.iter().zip(&hits).zip(&expected) {
        let seen = *h as f64 / trials as f64;
        assert!(
            (expected - seen).abs() < 0.01,
            "item {} of repo {}: expected {expected:.4}, saw {seen:.4}",
            c.item.issue.unwrap(),
            c.workspace_idx
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
