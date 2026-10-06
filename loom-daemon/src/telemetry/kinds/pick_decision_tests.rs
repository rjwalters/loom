//! `pick.decision` payload tests (#10212): ordering, cap + total, empty tick,
//! skip-reason mapping.

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
fn candidates_are_capped_with_the_uncapped_total() {
    let ranked: Vec<_> = (1..=80)
        .map(|n| (candidate(n, 1000 + n), PickVerdict::Skipped(PickSkipReason::Cap)))
        .collect();
    let record = PickDecisionRecord::build(tick("work_finder"), ranked);
    assert_eq!(record.candidates.len(), MAX_PICK_CANDIDATES);
    assert_eq!(record.candidates_total, 80);
    assert_eq!(record.candidates.last().unwrap().rank, 50);
    // Skips are reported only for listed candidates.
    assert_eq!(record.skipped.len(), MAX_PICK_CANDIDATES);
}

#[test]
fn an_acted_item_past_the_cap_is_still_reported() {
    let mut ranked: Vec<_> = (1..=60)
        .map(|n| (candidate(n, n), PickVerdict::Skipped(PickSkipReason::Cap)))
        .collect();
    ranked.push((candidate(61, 61), PickVerdict::Acted("dispatched")));
    let record = PickDecisionRecord::build(tick("work_finder"), ranked);
    assert_eq!(record.acted.len(), 1);
    assert_eq!(record.acted[0].number, 61);
    assert_eq!(record.candidates_total, 61);
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
fn pick_decision_record_size_is_bounded() {
    let empty =
        PickDecisionRecord::build(tick("judge"), Vec::new()).with_source(source::NONE, false);
    let empty_len = serde_json::to_string(&empty).unwrap().len();
    assert!(empty_len < 400, "empty tick is {empty_len} bytes");
    // Worst case: every list at the cap, long sort keys, plus extra actions.
    let ranked: Vec<_> = (1..=200)
        .map(|n| {
            let mut c = candidate(n, 100_000 + n);
            c.repo = "some-organisation/some-repository".to_string();
            c.sort_key = Some(PickSortKey {
                name: "pr_queue".to_string(),
                value: "level=2,reason=operator-priority,origin=interactive,mode=workflow"
                    .to_string(),
            });
            let verdict = if n % 2 == 0 {
                PickVerdict::Acted("changes_requested")
            } else {
                PickVerdict::Skipped(PickSkipReason::NotSelected)
            };
            (c, verdict)
        })
        .collect();
    let mut full =
        PickDecisionRecord::build(tick("judge"), ranked).with_source(source::SERVING_QUEUE, true);
    full.add_actions((0..100).map(|n| PickAction {
        repo: "some-organisation/some-repository".to_string(),
        number: 200_000 + n,
        action: "claimed".to_string(),
    }));
    assert_eq!(full.acted.len(), MAX_PICK_CANDIDATES);
    let full_len = serde_json::to_string(&full).unwrap().len();
    assert!(full_len < 20_000, "a full record is {full_len} bytes");
}

#[test]
fn role_actions_are_unique_wire_names() {
    let mut seen = std::collections::HashSet::new();
    for action in ROLE_ACTIONS {
        assert!(seen.insert(action), "duplicate {action}");
    }
    assert!(!seen.contains("dispatched"), "the work finder's action is separate");
}
