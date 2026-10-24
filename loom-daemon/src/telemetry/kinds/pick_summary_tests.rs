//! Summary tests (#12974): the 16 KB ceiling on a worst-case record, the
//! counts that replace the lists, and the unchanged totals.

use chrono::{TimeZone, Utc};

use super::*;
use crate::telemetry::kinds::pick_decision::{
    PickCandidate, PickDraw, PickDrawCandidate, PickItemDrawStep, PickItemExclusion,
    PickItemPoolEntry, PickItemRepo, PickSkipReason, PickTick, PickVerdict, PickWorkspaceDraw,
};

fn tick() -> PickTick {
    let at = Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap();
    PickTick {
        role: "work_finder".into(),
        host: "host-a".into(),
        tick_id: "work_finder-77".into(),
        started_at: at,
        ended_at: at,
        outcome: "dispatched".into(),
    }
}

fn item_draw(pools: u32, draws: u32) -> PickItemDraw {
    PickItemDraw {
        seed_scheme: "loom-draw/v3".into(),
        seed_minute: "29000000".into(),
        seed_host: "host-a".into(),
        tick_counter: "5".into(),
        draws_total: draws as usize,
        backlog_exponent: "0.6".into(),
        busy_exponent: "0.5".into(),
        repos: (0..50)
            .map(|r| PickItemRepo {
                repo: format!("o/r{r}"),
                weight: 1,
                in_flight: 0,
                items: 10,
                pool_weight: 100,
            })
            .collect(),
        closures: Vec::new(),
        excluded: vec![
            PickItemExclusion {
                repo: "o/r1".into(),
                reason: "frozen".into(),
                skip_reason: PickSkipReason::OperatorHold,
                count: 3,
            },
            PickItemExclusion {
                repo: "o/r2".into(),
                reason: "hard_exclusion".into(),
                skip_reason: PickSkipReason::OperatorHold,
                count: 4,
            },
        ],
        pools: (0..pools)
            .map(|i| PickItemPoolEntry {
                repo: "o/r1".into(),
                index: i,
                number: i,
                issue: Some(i),
                pr: None,
                step: "builder".into(),
                weight: 7,
                priority: 1,
                age_permille: 1000,
                age_source: "mirror".into(),
            })
            .collect(),
        draws: (0..draws)
            .map(|i| PickItemDrawStep {
                repo: "o/r1".into(),
                draw_index: i,
                seed: "123456789".into(),
                pool_size: 10,
                total_weight: 100,
                roll: 5,
                picked_index: 0,
                number: i,
                issue: Some(i),
                pr: None,
                step: "builder".into(),
                weight: 7,
                draw_weight: 7,
                repo_weight: 1,
                repo_items: 10,
                repo_pool_weight: 70,
                in_flight_repo: 0,
                actionable: 1,
                priority: 1,
                level: "normal".into(),
                lane: "normal".into(),
                age_permille: 1000,
                age_minutes: 60,
                age_source: "mirror".into(),
            })
            .collect(),
        weight_overflow: Vec::new(),
    }
}

/// Hundreds of candidates, 100+ draws of both kinds, and an oversized free-form remainder.
pub(crate) fn worst_case() -> PickDecisionRecord {
    let ranked = (0..400u32)
        .map(|n| {
            (
                PickCandidate {
                    rank: n + 1,
                    repo: format!("o/r{}", n % 50),
                    number: n,
                    stage: "loom:issue".into(),
                    sort_key: None,
                },
                match n % 3 {
                    0 => PickVerdict::Acted("claimed"),
                    1 => PickVerdict::Skipped(PickSkipReason::InFlight),
                    _ => PickVerdict::Skipped(PickSkipReason::Quota),
                },
            )
        })
        .collect();
    let draws = (0..150u32)
        .map(|d| PickDraw {
            candidates: (0..20)
                .map(|c| PickDrawCandidate {
                    repo: format!("o/w{c}"),
                    weight: 10,
                    base_weight: Some(1),
                    bonus: None,
                })
                .collect(),
            total_weight: 200,
            roll: 3,
            picked: "o/w1".into(),
            number: d,
        })
        .collect();
    let mut record = PickDecisionRecord::build(tick(), ranked)
        .with_item_draw(Some(item_draw(300, 120)))
        .with_workspace_draw(Some(PickWorkspaceDraw {
            seed: "42".into(),
            draws_total: 150,
            draws,
            seed_scheme: Some("loom-draw/v1".into()),
            seed_minute: Some("29000000".into()),
            seed_host: Some("host-a".into()),
        }))
        .with_dispatch_result(Some("dispatched"));
    record.lanes = (0..500)
        .map(|i| crate::telemetry::kinds::pick_decision::PickLane {
            repo: format!("o/lane-repo-{i}"),
            debt: 1,
            wanted: 1,
            lanes: 1,
        })
        .collect();
    record
}

#[test]
fn a_worst_case_summary_stays_under_the_ceiling() {
    let full = worst_case();
    assert!(serde_json::to_vec(&full).unwrap().len() > 10 * SUMMARY_MAX_BYTES);
    let summary = full.summarize("work_finder-77");
    let bytes = serde_json::to_vec(&summary).unwrap().len();
    assert!(bytes <= SUMMARY_MAX_BYTES, "{bytes} bytes");
}

#[test]
fn the_summary_carries_totals_and_counts_but_no_lists() {
    let full = worst_case();
    let s = full.summarize("work_finder-77");
    assert_eq!(s.payload_ref.as_deref(), Some("work_finder-77"));
    assert_eq!(s.candidates_total, 400);
    assert!(s.candidates.is_empty() && s.skipped.is_empty());
    assert_eq!(s.skipped_counts["in_flight"] + s.skipped_counts["quota"], 266);
    assert_eq!(s.exclusion_counts["operator_hold"], 7);
    assert_eq!(s.workspace_draw.as_ref().unwrap().draws_total, 150);
    assert!(s.workspace_draw.as_ref().unwrap().draws.is_empty());
    let item = s.item_draw.as_ref().unwrap();
    assert_eq!(item.draws_total, 120);
    assert!(item.pools.is_empty() && item.draws.is_empty() && item.excluded.is_empty());
    assert_eq!(item.seed_minute, "29000000");
    assert_eq!(s.dispatch_result.as_deref(), Some("dispatched"));
    assert_eq!(
        (s.role.as_str(), s.host.as_str(), s.outcome.as_str()),
        ("work_finder", "host-a", "dispatched")
    );
    // The full record is untouched and carries no payload_ref.
    assert!(full.payload_ref.is_none() && full.candidates.len() == 400);
}

#[test]
fn an_empty_tick_summarizes_to_itself_plus_the_ref() {
    let empty = PickDecisionRecord::build(tick(), Vec::new());
    let s = empty.summarize("work_finder-77");
    let mut expected = empty;
    expected.payload_ref = Some("work_finder-77".into());
    assert_eq!(s, expected);
}

#[test]
fn a_summary_round_trips_through_json_and_an_older_reader_shape() {
    let s = worst_case().summarize("work_finder-77");
    let back: PickDecisionRecord =
        serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
    assert_eq!(back, s);
    // Additive: a full record serializes none of the new fields.
    let json = serde_json::to_value(PickDecisionRecord::build(tick(), Vec::new())).unwrap();
    for k in ["payload_ref", "skipped_counts", "exclusion_counts"] {
        assert!(json.get(k).is_none(), "{k}");
    }
    assert_eq!(json["schema_version"], 2);
}
