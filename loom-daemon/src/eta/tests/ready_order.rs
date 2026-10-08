//! Ready rows this host's planner does not position (#10903): the pure
//! placement in `eta::ready_order`, and the tracker estimating them.

use super::ready::history_ready;
use super::{as_of, provenance};
use crate::eta::ready_order::{placements, Placed, Placement};
use crate::eta::tracker::{EstimateContext, IssueRow, ReadyPlan, ReadyRow, Tracker};
use crate::eta::{DispatchInput, Kind, NoEstimateReason, Registry};
use crate::types::{
    DispatchPlanContext, PlanGate, PlanSlots, PlanState, QueueDisposition, RowPlan,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

/// A waiting row the planner positioned.
fn waiting(issue: u32, rank: usize, position: u32) -> ReadyRow {
    ReadyRow {
        repo: REPO.to_string(),
        issue,
        rank,
        plan: RowPlan {
            plan_state: PlanState::Queued,
            position: Some(position),
            gate: Some(PlanGate::Capacity),
            ..RowPlan::default()
        },
        disposition: QueueDisposition::DeferredCapacity,
        detail: None,
        facts: IssueRow::default(),
    }
}

/// A `blocked` row with no position.
fn blocked(issue: u32, rank: usize, disposition: QueueDisposition) -> ReadyRow {
    ReadyRow {
        repo: REPO.to_string(),
        issue,
        rank,
        plan: RowPlan {
            plan_state: PlanState::Blocked,
            ..RowPlan::default()
        },
        disposition,
        detail: None,
        facts: IssueRow::default(),
    }
}

fn with_detail(mut row: ReadyRow, detail: &str) -> ReadyRow {
    row.detail = Some(detail.to_string());
    row
}

fn held(mut row: ReadyRow, until: DateTime<Utc>) -> ReadyRow {
    row.plan.held_until = Some(until);
    row
}

fn placed(position: u32, ahead: u32, not_here: &str) -> Placement {
    Placement::Placed(Placed {
        position,
        ahead,
        not_here: not_here.to_string(),
        held_until: None,
    })
}

// ---- the pure placement ----

#[test]
fn a_row_only_this_host_cannot_dispatch_is_placed_by_comparator_rank() {
    // Planner order (shaped) is not rank order: rank 5 sits at position 2.
    let rows = [
        waiting(10, 1, 1),
        waiting(11, 5, 2),
        waiting(12, 2, 3),
        blocked(20, 3, QueueDisposition::HostConstraint),
        blocked(21, 9, QueueDisposition::PeerClaim),
        blocked(22, 0, QueueDisposition::WorkspaceCommandsMissing),
    ];
    let got = placements(&rows);
    assert_eq!(&got[..3], &[Placement::Waiting, Placement::Waiting, Placement::Waiting]);
    // Rank 3: ranks 1 and 2 are ahead (positions 1 and 3), so it takes
    // position 4, behind the three rows at positions 1..=3.
    assert_eq!(got[3], placed(4, 3, "host_constraint"));
    // Rank 9: every waiting row is ahead.
    assert_eq!(got[4], placed(4, 3, "peer_claim"));
    // Rank 0: nothing ahead, so it takes position 1.
    assert_eq!(got[5], placed(1, 0, "workspace_commands_missing"));
}

#[test]
fn a_placed_row_is_never_ahead_of_a_waiting_row_ranked_before_it() {
    // The shaped order: low rank, high position (the issue-review
    // counterexample), plus a placed row between them.
    let rows = [
        waiting(10, 1, 3),
        waiting(11, 8, 1),
        waiting(12, 9, 2),
        blocked(20, 5, QueueDisposition::HostConstraint),
        blocked(21, 10, QueueDisposition::PeerClaim),
        blocked(22, 2, QueueDisposition::DispatchError),
    ];
    let got = placements(&rows);
    for (i, p) in rows.iter().enumerate().filter(|(i, _)| *i >= 3) {
        let Placement::Placed(placed_row) = &got[i] else {
            panic!("row {} not placed", p.issue);
        };
        // `ahead` agrees with the slot the row takes.
        assert_eq!(placed_row.position, placed_row.ahead + 1, "row {}", p.issue);
        for (w, wr) in rows.iter().enumerate().take(3) {
            let wp = wr.plan.position.unwrap();
            let w_ahead = rows[..3]
                .iter()
                .filter(|r| r.plan.position.unwrap() < wp)
                .count() as u32;
            if wr.rank < p.rank {
                assert!(
                    placed_row.ahead > w_ahead,
                    "row {} (ahead {}) vs waiting {} (ahead {w_ahead})",
                    p.issue,
                    placed_row.ahead,
                    rows[w].issue
                );
            }
        }
    }
    assert_eq!(got[3], placed(4, 3, "host_constraint"));
}

#[test]
fn placed_rows_never_count_each_other_and_need_no_waiting_rows() {
    let rows = [
        blocked(20, 1, QueueDisposition::HostClassRefused),
        blocked(21, 2, QueueDisposition::DispatchError),
    ];
    assert_eq!(
        placements(&rows),
        vec![
            placed(1, 0, "host_class_refused"),
            placed(1, 0, "dispatch_error")
        ]
    );
}

#[test]
fn a_host_local_halt_is_placed_and_a_red_main_is_refused() {
    let halted =
        |cause: &str| with_detail(blocked(20, 1, QueueDisposition::WorkspaceHalted), cause);
    for cause in [
        "token_pool",
        "breaker",
        "drain",
        "gate_pending",
        "preflight_advisory",
        "write_scope",
    ] {
        assert_eq!(
            placements(&[halted(cause)]),
            vec![placed(1, 0, &format!("workspace_halted:{cause}"))],
            "{cause}"
        );
    }
    for cause in ["main_red", "ci_billing", "not-a-cause"] {
        assert_eq!(
            placements(&[halted(cause)]),
            vec![Placement::Refused(NoEstimateReason::NoDispatchPlan)],
            "{cause}"
        );
    }
    let causeless = blocked(20, 1, QueueDisposition::WorkspaceHalted);
    assert_eq!(
        placements(&[causeless]),
        vec![Placement::Refused(NoEstimateReason::NoDispatchPlan)]
    );
}

#[test]
fn a_time_boxed_hold_is_placed_with_its_expiry_and_one_without_is_refused() {
    let until = as_of() + Duration::minutes(30);
    for disposition in [
        QueueDisposition::DispatchBackoff,
        QueueDisposition::NoopCooldown,
        QueueDisposition::RecheckInterval,
        QueueDisposition::PrlessRetry,
    ] {
        let got = placements(&[waiting(10, 1, 1), held(blocked(20, 2, disposition), until)]);
        assert_eq!(
            got[1],
            Placement::Placed(Placed {
                position: 2,
                ahead: 1,
                not_here: disposition.as_str().to_string(),
                held_until: Some(until),
            }),
            "{disposition:?}"
        );
        assert_eq!(
            placements(&[blocked(20, 2, disposition)]),
            vec![Placement::Refused(NoEstimateReason::NoDispatchPlan)],
            "{disposition:?} with no clock"
        );
    }
}

#[test]
fn holds_and_issue_level_refusals_keep_their_reason() {
    for (disposition, reason) in [
        (QueueDisposition::Parked, NoEstimateReason::Blocked),
        (QueueDisposition::HardExclusion, NoEstimateReason::Blocked),
        (QueueDisposition::LabelledBlocked, NoEstimateReason::Blocked),
        (QueueDisposition::Quarantined, NoEstimateReason::NoDispatchPlan),
        (QueueDisposition::OpenPr, NoEstimateReason::NoDispatchPlan),
        (QueueDisposition::OpenPrBackoff, NoEstimateReason::NoDispatchPlan),
        (QueueDisposition::Declined, NoEstimateReason::NoDispatchPlan),
        (QueueDisposition::DeferredCapacity, NoEstimateReason::NoDispatchPlan),
        (QueueDisposition::Unknown, NoEstimateReason::NoDispatchPlan),
    ] {
        // Even with an expiry, none of these is placed.
        let row = held(blocked(20, 1, disposition), as_of());
        assert_eq!(placements(&[row]), vec![Placement::Refused(reason)], "{disposition:?}");
    }
    let mut running = blocked(30, 1, QueueDisposition::InFlight);
    running.plan.plan_state = PlanState::Running;
    assert_eq!(placements(&[running]), vec![Placement::Running]);
}

#[test]
fn a_held_until_hold_adds_its_remaining_time_to_the_admission_delay() {
    let plan_at = as_of();
    let base = DispatchInput {
        position: 1,
        plan_state: "queued".to_string(),
        gate: None,
        ahead: 0,
        free_slots: 0,
        max_admissions_per_tick: Some(2),
        tick_interval_secs: 60,
        saturation_held: false,
        plan_at,
        not_here: None,
        held_until: None,
    };
    assert_eq!(base.admission_delay_sec(), 30);
    let held = DispatchInput {
        held_until: Some(plan_at + Duration::seconds(1800)),
        ..base.clone()
    };
    assert_eq!(held.admission_delay_sec(), 30 + 1800);
    let lapsed = DispatchInput {
        held_until: Some(plan_at - Duration::seconds(5)),
        ..base
    };
    assert_eq!(lapsed.admission_delay_sec(), 30, "an expired hold adds nothing");
}

// ---- the tracker ----

fn plan() -> ReadyPlan {
    ReadyPlan {
        context: DispatchPlanContext {
            slots: PlanSlots {
                max_concurrent: 4,
                occupancy: Some(4),
                free: Some(0),
                max_admissions_per_tick: Some(2),
                ..PlanSlots::default()
            },
            tick_interval_secs: Some(60),
            complete: true,
            ..DispatchPlanContext::default()
        },
        at: as_of() - Duration::seconds(10),
        listing_failed: Vec::new(),
    }
}

fn estimate(tracker: &mut Tracker) -> Vec<crate::eta::tracker::Emission> {
    let history = history_ready();
    let registry = Registry::builtin();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &crate::eta::stall::StallSnapshot::default(),
    };
    tracker.estimate(None, &ctx, as_of())
}

fn primary(
    emissions: &[crate::eta::tracker::Emission],
    issue: u32,
    kind: Kind,
) -> crate::eta::explanation::Explanation {
    emissions
        .iter()
        .find(|e| e.primary && e.explanation.subject.issue == issue && e.explanation.kind == kind)
        .map(|e| e.explanation.clone())
        .unwrap_or_else(|| panic!("{issue} {kind}"))
}

fn dispatch_of(e: &crate::eta::explanation::Explanation) -> DispatchInput {
    e.path
        .as_ref()
        .and_then(|p| p.dispatch.as_ref())
        .map(|d| d.input.clone())
        .unwrap_or_else(|| panic!("{} has no path.dispatch", e.subject.issue))
}

#[test]
fn the_authority_estimates_rows_it_cannot_dispatch_and_leaves_waiting_rows_alone() {
    let backoff_until = as_of() + Duration::seconds(1800);
    let waiting_rows = [waiting(10, 1, 1), waiting(11, 4, 2)];
    let extra = [
        blocked(20, 2, QueueDisposition::HostConstraint),
        blocked(21, 3, QueueDisposition::PeerClaim),
        blocked(22, 5, QueueDisposition::WorkspaceCommandsMissing),
        with_detail(blocked(23, 6, QueueDisposition::WorkspaceHalted), "token_pool"),
        held(blocked(24, 7, QueueDisposition::DispatchBackoff), backoff_until),
        with_detail(blocked(25, 8, QueueDisposition::Parked), "loom:blocked"),
        with_detail(blocked(26, 9, QueueDisposition::WorkspaceHalted), "main_red"),
    ];

    // Baseline: the waiting rows alone.
    let mut alone = Tracker::new(provenance());
    alone.on_ready_queue(&waiting_rows, &plan(), as_of());
    let alone = estimate(&mut alone);

    let mut tracker = Tracker::new(provenance());
    let rows: Vec<ReadyRow> = waiting_rows.iter().chain(extra.iter()).cloned().collect();
    tracker.on_ready_queue(&rows, &plan(), as_of());
    let emissions = estimate(&mut tracker);

    // Waiting rows: byte-identical inputs and numbers with or without the
    // placed rows beside them.
    for issue in [10, 11] {
        for kind in [Kind::Start, Kind::Land] {
            let before = primary(&alone, issue, kind);
            let after = primary(&emissions, issue, kind);
            assert_eq!(dispatch_of(&before), dispatch_of(&after), "{issue} {kind}");
            assert_eq!(before.quantiles(), after.quantiles(), "{issue} {kind}");
            assert_eq!(dispatch_of(&after).not_here, None);
        }
    }

    // Placed rows: estimated, with the reason recorded, never refused.
    for (issue, not_here, ahead) in [
        (20, "host_constraint", 1),
        (21, "peer_claim", 1),
        (22, "workspace_commands_missing", 2),
        (23, "workspace_halted:token_pool", 2),
        (24, "dispatch_backoff", 2),
    ] {
        for kind in [Kind::Start, Kind::Land] {
            let e = primary(&emissions, issue, kind);
            assert_eq!(e.no_estimate_reason, None, "{issue} {kind}");
            assert!(e.quantiles().is_some(), "{issue} {kind}");
            let d = dispatch_of(&e);
            assert_eq!(d.not_here.as_deref(), Some(not_here), "{issue} {kind}");
            assert_eq!(d.ahead, ahead, "{issue} {kind}");
            assert_eq!(d.gate, None, "a placed row names a reason, not a gate");
        }
    }
    let backoff = primary(&emissions, 24, Kind::Start);
    assert_eq!(dispatch_of(&backoff).held_until, Some(backoff_until));
    let record = backoff.path.as_ref().unwrap().dispatch.as_ref().unwrap();
    assert!(record.admission_delay_sec >= 1800, "{}", record.admission_delay_sec);
    let (_, held_p50, _) = backoff.quantiles().unwrap();
    let (_, free_p50, _) = primary(&emissions, 22, Kind::Start).quantiles().unwrap();
    assert!(held_p50 > free_p50, "the hold moves the start: {held_p50} vs {free_p50}");

    // A hold label is `blocked`; a red `main` halts every host.
    for kind in [Kind::Start, Kind::Land] {
        assert_eq!(
            primary(&emissions, 25, kind).no_estimate_reason,
            Some(NoEstimateReason::Blocked)
        );
        assert_eq!(
            primary(&emissions, 26, kind).no_estimate_reason,
            Some(NoEstimateReason::NoDispatchPlan)
        );
    }
}

#[test]
fn a_placed_row_whose_reason_changes_is_re_estimated() {
    let mut tracker = Tracker::new(provenance());
    let first = tracker.on_ready_queue(
        &[
            waiting(10, 1, 1),
            blocked(20, 2, QueueDisposition::PeerClaim),
        ],
        &plan(),
        as_of(),
    );
    assert!(first.dirty.iter().any(|k| k.issue == 20));
    let same = tracker.on_ready_queue(
        &[
            waiting(10, 1, 1),
            blocked(20, 2, QueueDisposition::PeerClaim),
        ],
        &plan(),
        as_of(),
    );
    assert!(same.dirty.is_empty(), "an unchanged placement is not dirty");
    let moved = tracker.on_ready_queue(
        &[
            waiting(10, 1, 1),
            blocked(20, 2, QueueDisposition::HostConstraint),
        ],
        &plan(),
        as_of(),
    );
    assert_eq!(moved.dirty.iter().map(|k| k.issue).collect::<Vec<_>>(), vec![20]);
}

#[test]
fn a_placed_rows_journal_row_carries_its_reason_for_backtests() {
    let mut tracker = Tracker::new(provenance());
    let effects = tracker.on_ready_queue(
        &[blocked(20, 2, QueueDisposition::HostConstraint)],
        &plan(),
        as_of(),
    );
    let row = &effects.journal[0];
    assert_eq!(row.raw["dispatch"]["not_here"], "host_constraint");
    // The journal's dispatch parses back as the input it was estimated from.
    let parsed: DispatchInput = serde_json::from_value(row.raw["dispatch"].clone()).unwrap();
    assert_eq!(parsed.not_here.as_deref(), Some("host_constraint"));
    // A positioned row's journal row is unchanged: no new keys.
    let effects = Tracker::new(provenance()).on_ready_queue(&[waiting(10, 1, 1)], &plan(), as_of());
    let dispatch = effects.journal[0].raw["dispatch"]
        .as_object()
        .unwrap()
        .clone();
    assert!(!dispatch.contains_key("not_here") && !dispatch.contains_key("held_until"));
}
