//! `pick.decision` payload tests (#10212): ordering, complete lists + total,
//! empty tick, skip-reason mapping, and the chunk split (#11567).

use super::*;
use chrono::TimeZone;

fn tick(role: &str) -> PickTick {
    let at = Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap();
    PickTick {
        role: role.to_string(),
        host: "host-a".to_string(),
        tick_id: "tick-1".to_string(),
        started_at: at,
        ended_at: at,
        outcome: "success".to_string(),
    }
}

fn candidate(rank: u32, number: u32) -> PickCandidate {
    PickCandidate {
        rank,
        repo: "o/r".to_string(),
        number,
        stage: "loom:review-requested".to_string(),
        sort_key: Some(PickSortKey {
            name: "listing_order".to_string(),
            value: rank.to_string(),
        }),
    }
}

#[test]
fn ranking_order_is_preserved_not_resorted() {
    // Deliberately not numeric order: the ranker's order wins.
    let ranked = vec![
        (candidate(1, 30), PickVerdict::Undecided),
        (candidate(2, 10), PickVerdict::Undecided),
        (candidate(3, 20), PickVerdict::Undecided),
    ];
    let record = PickDecisionRecord::build(tick("judge"), ranked);
    let numbers: Vec<u32> = record.candidates.iter().map(|c| c.number).collect();
    assert_eq!(numbers, vec![30, 10, 20]);
    assert_eq!(record.candidates_total, 3);
}

#[test]
fn every_candidate_is_listed_with_its_verdict() {
    let ranked: Vec<_> = (1..=80)
        .map(|n| (candidate(n, 1000 + n), PickVerdict::Skipped(PickSkipReason::Cap)))
        .collect();
    let record = PickDecisionRecord::build(tick("work_finder"), ranked);
    assert_eq!(record.candidates.len(), 80);
    assert_eq!(record.candidates_total, 80);
    assert_eq!(record.candidates.last().unwrap().rank, 80);
    // Every candidate's decision is carried: one skip per candidate, in rank
    // order.
    let skip_nums: Vec<u32> = record.skipped.iter().map(|s| s.number).collect();
    let cand_nums: Vec<u32> = record.candidates.iter().map(|c| c.number).collect();
    assert_eq!(skip_nums, cand_nums);
}

#[test]
fn an_acted_item_deep_in_the_ranking_is_still_reported() {
    let mut ranked: Vec<_> = (1..=60)
        .map(|n| (candidate(n, n), PickVerdict::Skipped(PickSkipReason::Cap)))
        .collect();
    ranked.push((candidate(61, 61), PickVerdict::Acted("dispatched")));
    let record = PickDecisionRecord::build(tick("work_finder"), ranked);
    assert_eq!(record.acted.len(), 1);
    assert_eq!(record.acted[0].number, 61);
    assert_eq!(record.candidates_total, 61);
    assert_eq!(record.candidates.len(), 61, "no list cap: all 61 listed");
}

#[test]
fn an_empty_tick_is_a_valid_record() {
    let record = PickDecisionRecord::build(tick("champion"), Vec::new());
    assert_eq!(record.candidates_total, 0);
    assert!(record.candidates.is_empty() && record.acted.is_empty() && record.skipped.is_empty());
    assert_eq!(record.schema_version, PICK_DECISION_SCHEMA_VERSION);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["candidates"], serde_json::json!([]));
    let back: PickDecisionRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back, record);
}

#[test]
fn verdicts_split_into_acted_and_skipped() {
    let ranked = vec![
        (candidate(1, 1), PickVerdict::Acted("dispatched")),
        (candidate(2, 2), PickVerdict::Skipped(PickSkipReason::PrOpenSkip)),
        (candidate(3, 3), PickVerdict::Undecided),
    ];
    let record = PickDecisionRecord::build(tick("work_finder"), ranked);
    assert_eq!(record.acted.len(), 1);
    assert_eq!(record.skipped.len(), 1);
    assert_eq!(record.skipped[0].reason, PickSkipReason::PrOpenSkip);
    assert_eq!(record.candidates.len(), 3);
}

#[test]
fn every_disposition_maps_and_only_dispatched_is_not_a_skip() {
    for d in QueueDisposition::ALL {
        let reason = PickSkipReason::from_disposition(d);
        assert_eq!(reason.is_none(), d == QueueDisposition::Dispatched, "{d:?}");
    }
    use QueueDisposition as Qd;
    let map = PickSkipReason::from_disposition;
    assert_eq!(map(Qd::OpenPr), Some(PickSkipReason::PrOpenSkip));
    assert_eq!(map(Qd::OpenPrBackoff), Some(PickSkipReason::PrOpenSkip));
    assert_eq!(map(Qd::Parked), Some(PickSkipReason::OperatorHold));
    assert_eq!(map(Qd::DeferredCapacity), Some(PickSkipReason::Cap));
    assert_eq!(map(Qd::DeferredRepoCap), Some(PickSkipReason::Cap));
    assert_eq!(map(Qd::InFlight), Some(PickSkipReason::InFlight));
    assert_eq!(map(Qd::LabelledBlocked), Some(PickSkipReason::Blocked));
}

#[test]
fn reason_wire_names_match_serde_and_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for reason in PickSkipReason::ALL {
        assert_eq!(serde_json::to_value(reason).unwrap(), serde_json::json!(reason.as_str()));
        assert!(seen.insert(reason.as_str()), "duplicate {reason:?}");
    }
    // The five the issue names are all present.
    for name in [
        "overlap_chain",
        "pr_open_skip",
        "operator_hold",
        "quota",
        "cap",
    ] {
        assert!(seen.contains(name), "{name}");
    }
}

#[test]
fn an_empty_tick_is_small_and_an_under_budget_record_is_a_single_chunk() {
    let empty =
        PickDecisionRecord::build(tick("judge"), Vec::new()).with_source(source::NONE, false);
    let empty_len = serde_json::to_string(&empty).unwrap().len();
    assert!(empty_len < 400, "empty tick is {empty_len} bytes");

    let under =
        PickDecisionRecord::build(tick("judge"), vec![(candidate(1, 1), PickVerdict::Undecided)])
            .with_source(source::SERVING_QUEUE, true);
    let chunks = split_into_chunks(under.clone(), CHUNK_BYTES);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0].chunk_index, chunks[0].chunk_count),
        (0, 1),
        "a record that fits carries 0/1"
    );
    assert_eq!(chunks[0], under);
}

/// A busy work-finder tick (#11567): hundreds of candidates and draws. Every
/// list round-trips through the chunk split complete, and every chunk is
/// within the OTLP-safe byte budget.
#[test]
fn a_busy_tick_splits_into_bounded_chunks_and_reassembles_exactly() {
    let ranked: Vec<_> = (1..=575)
        .map(|n| {
            let mut c = candidate(n, 100_000 + n);
            c.repo = "some-organisation/some-repository".to_string();
            c.sort_key = Some(PickSortKey {
                name: "candidate_cmp".to_string(),
                value: "level=2,reason=priority,origin=interactive,mode=workflow".to_string(),
            });
            let verdict = if n % 2 == 0 {
                PickVerdict::Acted("dispatched")
            } else {
                PickVerdict::Skipped(PickSkipReason::NotSelected)
            };
            (c, verdict)
        })
        .collect();
    let mut record = PickDecisionRecord::build(tick("work_finder"), ranked)
        .with_source(source::READY_QUEUE, true);
    record.workspace_draw = Some(PickWorkspaceDraw {
        seed: "123456789".to_string(),
        draws_total: 400,
        draws: (0..400)
            .map(|d| PickDraw {
                candidates: (0..60)
                    .map(|w| PickDrawCandidate {
                        repo: format!("org-{w}/workspace-{w}"),
                        weight: 1000 + w,
                        base_weight: None,
                        bonus: None,
                    })
                    .collect(),
                total_weight: 60 * 1000,
                roll: d,
                picked: "org-0/workspace-0".to_string(),
                number: u32::try_from(100_000 + d).unwrap(),
            })
            .collect(),
        seed_scheme: None,
        seed_minute: None,
        seed_host: None,
    });
    let whole_len = serde_json::to_string(&record).unwrap().len();
    assert!(whole_len > CHUNK_BYTES, "a busy tick is {whole_len} bytes");

    let chunks = split_into_chunks(record.clone(), CHUNK_BYTES);
    assert!(chunks.len() > 1, "{whole_len} bytes split into >1 chunk");
    assert!(chunks
        .iter()
        .all(|c| usize::try_from(c.chunk_count) == Ok(chunks.len())));
    for chunk in &chunks {
        let len = serde_json::to_string(chunk).unwrap().len();
        assert!(len <= CHUNK_BYTES, "chunk {} is {len} bytes", chunk.chunk_index);
    }
    assert_eq!(reassemble(&chunks), record, "the union is the original");
}

#[test]
fn a_record_over_a_tight_budget_splits_and_keeps_order() {
    let ranked: Vec<_> = (1..=40)
        .map(|n| {
            let verdict = if n % 3 == 0 {
                PickVerdict::Acted("claimed")
            } else if n % 3 == 1 {
                PickVerdict::Skipped(PickSkipReason::Cap)
            } else {
                PickVerdict::Undecided
            };
            (candidate(n, n), verdict)
        })
        .collect();
    let mut record =
        PickDecisionRecord::build(tick("judge"), ranked).with_source(source::SERVING_QUEUE, true);
    record.very_important_demoted = (0..5)
        .map(|d| PickViDemotion {
            repo: "o/r".to_string(),
            number: 500 + d,
            holder: 400 + d,
        })
        .collect();
    let original_json = serde_json::to_string(&record).unwrap();

    let chunks = split_into_chunks(record.clone(), 2_000);
    assert!(chunks.len() > 1);
    assert_eq!(reassemble(&chunks), record);
    // Rank order survives the split.
    let ranks: Vec<u32> = reassemble(&chunks)
        .candidates
        .iter()
        .map(|c| c.rank)
        .collect();
    assert_eq!(ranks, (1..=40).collect::<Vec<_>>());
    assert_eq!(
        serde_json::to_string(&reassemble(&chunks)).unwrap(),
        original_json,
        "byte-identical after a round trip"
    );
    // Header fields repeat on every chunk.
    for chunk in &chunks {
        assert_eq!(chunk.role, record.role);
        assert_eq!(chunk.host, record.host);
        assert_eq!(chunk.tick_id, record.tick_id);
        assert_eq!(chunk.started_at, record.started_at);
        assert_eq!(chunk.ended_at, record.ended_at);
        assert_eq!(chunk.outcome, record.outcome);
        assert_eq!(chunk.candidates_total, record.candidates_total);
        assert_eq!(chunk.candidate_source, record.candidate_source);
        assert_eq!(chunk.decisions_observed, record.decisions_observed);
        assert_eq!(chunk.schema_version, record.schema_version);
    }
}

#[test]
fn draws_split_in_order_with_their_totals_on_every_chunk() {
    let record = PickDecisionRecord::build(tick("work_finder"), Vec::new()).with_workspace_draw(
        Some(PickWorkspaceDraw {
            seed: "42".to_string(),
            draws_total: 7,
            draws: (0u32..7)
                .map(|d| PickDraw {
                    candidates: vec![PickDrawCandidate {
                        repo: format!("o/w{d}"),
                        weight: u64::from(10 + d),
                        base_weight: None,
                        bonus: None,
                    }],
                    total_weight: u64::from(10 + d),
                    roll: u64::from(d),
                    picked: format!("o/w{d}"),
                    number: d,
                })
                .collect(),
            seed_scheme: Some("loom-draw/v1".to_string()),
            seed_minute: Some("29000000".to_string()),
            seed_host: Some("host-a".to_string()),
        }),
    );
    let chunks = split_into_chunks(record.clone(), 900);
    assert!(chunks.len() > 1);
    assert_eq!(reassemble(&chunks), record);
    assert_eq!(draws_in_order(&chunks), (0..7).collect::<Vec<_>>());
    for chunk in &chunks {
        let wd = chunk.workspace_draw.as_ref().unwrap();
        assert_eq!(wd.seed, "42");
        assert_eq!(wd.draws_total, 7, "totals repeat on every chunk");
        // #11683: the seed's inputs repeat like the seed.
        assert_eq!(wd.seed_scheme.as_deref(), Some("loom-draw/v1"));
        assert_eq!(wd.seed_minute.as_deref(), Some("29000000"));
        assert_eq!(wd.seed_host.as_deref(), Some("host-a"));
    }
}

fn draws_in_order(chunks: &[PickDecisionRecord]) -> Vec<u32> {
    let mut sorted: Vec<&PickDecisionRecord> = chunks.iter().collect();
    sorted.sort_by_key(|c| c.chunk_index);
    sorted
        .iter()
        .filter_map(|c| c.workspace_draw.as_ref())
        .flat_map(|d| d.draws.iter().map(|d| d.number))
        .collect()
}

/// Concatenate the chunks' lists in `chunk_index` order.
fn reassemble(chunks: &[PickDecisionRecord]) -> PickDecisionRecord {
    let mut sorted: Vec<PickDecisionRecord> = chunks.to_vec();
    sorted.sort_by_key(|c| c.chunk_index);
    let mut out = sorted[0].clone();
    // The union is the unsplit record: restore its chunk stamp.
    out.chunk_index = 0;
    out.chunk_count = 1;
    out.candidates = sorted
        .iter()
        .flat_map(|c| c.candidates.iter().cloned())
        .collect();
    out.skipped = sorted
        .iter()
        .flat_map(|c| c.skipped.iter().cloned())
        .collect();
    out.acted = sorted
        .iter()
        .flat_map(|c| c.acted.iter().cloned())
        .collect();
    out.very_important_demoted = sorted
        .iter()
        .flat_map(|c| c.very_important_demoted.iter().cloned())
        .collect();
    out.workspace_draw = sorted
        .iter()
        .find(|c| c.workspace_draw.is_some())
        .map(|header| {
            let mut wd = header.workspace_draw.clone().expect("checked Some");
            wd.draws = sorted
                .iter()
                .filter_map(|c| c.workspace_draw.as_ref())
                .flat_map(|d| d.draws.iter().cloned())
                .collect();
            wd
        });
    out
}

#[test]
fn role_actions_are_unique_wire_names() {
    let mut seen = std::collections::HashSet::new();
    for action in ROLE_ACTIONS {
        assert!(seen.insert(action), "duplicate {action}");
    }
    assert!(!seen.contains("dispatched"), "the work finder's action is separate");
}

/// #11683: a record written before the seed inputs existed still parses,
/// and a draw without them serializes none of the three fields.
#[test]
fn workspace_draw_without_seed_inputs_round_trips() {
    let old = serde_json::json!({"seed": "42", "draws_total": 0, "draws": []});
    let draw: PickWorkspaceDraw = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(draw.seed_scheme, None);
    assert_eq!(draw.seed_minute, None);
    assert_eq!(draw.seed_host, None);
    assert_eq!(serde_json::to_value(&draw).unwrap(), old);
}
