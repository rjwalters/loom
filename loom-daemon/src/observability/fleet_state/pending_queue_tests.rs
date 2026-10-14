//! Per-repo pending-human queue depth on `fleet.state` (#11955).

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};

use super::sources::is_eligible;
use super::{
    annotate_pending, decide, operator_held, Emitted, FleetHoldKind, FleetPrCensus,
    FleetQueueDepth, FleetStage, FleetStateRow, FleetView, PlannerStamps, RepoView,
};

const REPO: &str = "acme/a";

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn census(merge_hold: u32) -> FleetPrCensus {
    FleetPrCensus {
        open: merge_hold,
        by_stage: BTreeMap::from([("merge_hold".to_string(), merge_hold)]),
    }
}

fn eligible(curation: u32, approval: u32) -> BTreeMap<String, (u32, u32)> {
    BTreeMap::from([(REPO.to_string(), (curation, approval))])
}

#[test]
fn skip_park_and_proposal_labels_are_ineligible() {
    assert!(is_eligible(&["loom:curated", "loom:important"]));
    assert!(is_eligible::<&str>(&[]));
    for label in [
        "loom:skip",
        "loom:park",
        "loom:architect",
        "loom:hermit",
        "loom:auditor",
    ] {
        assert!(!is_eligible(&["loom:triage", label]), "{label}");
    }
}

#[test]
fn sub_counts_are_stamped_with_operator_held_from_rows_and_census() {
    let mut view = FleetView::default();
    let state = view
        .repos
        .entry(REPO.to_string())
        .or_insert_with(RepoView::default);
    state.census = Some(census(2));
    // One merge_hold row (issue 1) and one operator-held review row (issue 2).
    state
        .rows
        .insert(1, FleetStateRow::new(1, FleetStage::MergeHold, t0()));
    let mut held = FleetStateRow::new(2, FleetStage::MergeWait, t0());
    held.hold_kind = Some(FleetHoldKind::Operator);
    state.rows.insert(2, held);
    annotate_pending(&mut view, &eligible(4, 7));
    // Row 1 is counted once though it is both a row and in the census; the
    // census's second merge_hold PR has no row, so it adds one.
    assert_eq!(
        view.repos[REPO].pending_queue,
        Some(FleetQueueDepth {
            awaiting_curation: 4,
            awaiting_approval: 7,
            operator_held: Some(3),
        })
    );
}

#[test]
fn a_blocked_hold_is_not_an_operator_hold() {
    let mut state = RepoView {
        census: Some(FleetPrCensus::default()),
        ..RepoView::default()
    };
    let mut row = FleetStateRow::new(1, FleetStage::MergeWait, t0());
    row.hold_kind = Some(FleetHoldKind::Blocked);
    state.rows.insert(1, row);
    assert_eq!(operator_held(&state), Some(0));
}

#[test]
fn no_census_leaves_operator_held_unknown_and_missing_listing_leaves_none() {
    let mut view = FleetView::default();
    view.repos.insert("acme/b".to_string(), RepoView::default());
    annotate_pending(&mut view, &eligible(1, 2));
    assert_eq!(view.repos[REPO].pending_queue.unwrap().operator_held, None);
    // A repo absent from the eligible map (incomplete listing) is unknown.
    assert_eq!(view.repos["acme/b"].pending_queue, None);
}

#[test]
fn a_change_in_pending_queue_is_a_delta_and_unchanged_is_not() {
    let stamps = PlannerStamps {
        planner_version: "v".to_string(),
        planner_config_hash: "h".to_string(),
        fleet_config_hash: None,
    };
    let mut view = FleetView::default();
    annotate_pending(&mut view, &eligible(1, 2));
    let anchor = decide(&view, &stamps, None, t0()).unwrap();
    assert!(anchor.anchor);
    let last = Emitted {
        view: view.clone(),
        anchor_as_of: t0(),
        as_of: t0(),
        stamps: stamps.clone(),
    };
    let later = t0() + chrono::Duration::seconds(60);
    assert!(decide(&view, &stamps, Some(&last), later).is_none(), "unchanged");
    let mut changed = view.clone();
    annotate_pending(&mut changed, &eligible(1, 3));
    let delta = decide(&changed, &stamps, Some(&last), later).unwrap();
    assert!(!delta.anchor);
    assert_eq!(delta.repos[0].pending_queue.unwrap().awaiting_approval, 3);
}

#[test]
fn legacy_record_without_the_field_decodes() {
    let repo: crate::telemetry::kinds::fleet_state::FleetStateRepo = serde_json::from_str(
        r#"{"repo":"a/b","visibility":"public","ready_complete":false,"ready_replace":true}"#,
    )
    .unwrap();
    assert_eq!(repo.pending_queue, None);
    let with = FleetQueueDepth {
        awaiting_curation: 1,
        awaiting_approval: 2,
        operator_held: None,
    };
    let json = serde_json::to_string(&with).unwrap();
    assert!(!json.contains("operator_held"));
    assert_eq!(serde_json::from_str::<FleetQueueDepth>(&json).unwrap(), with);
}
