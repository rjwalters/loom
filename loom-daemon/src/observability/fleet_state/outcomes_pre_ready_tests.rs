//! Pre-ready stage exits (#11368), beside the shared helpers of `outcomes_tests`.

use super::*;

// ---- Pre-ready exits (#11368) ----

/// The curated listing of `REPO` holding `issues`, each created at `created`.
fn curated(issues: &[u32], created: DateTime<Utc>) -> Curated {
    [(REPO.to_string(), issues.iter().map(|n| (*n, Some(created))).collect())].into()
}

fn minutes(m: i64) -> DateTime<Utc> {
    t0() + Duration::minutes(m)
}

fn pre_ready(records: &[TelemetryRecord]) -> Vec<&StageOutcomeRecord> {
    stages(records)
        .into_iter()
        .filter(|r| matches!(r.stage, FleetStage::TriageWait | FleetStage::ApprovalWait))
        .collect()
}

fn assert_exit(
    r: &StageOutcomeRecord,
    stage: FleetStage,
    (entered, left): (DateTime<Utc>, DateTime<Utc>),
    next: FleetStage,
) {
    assert_eq!((r.stage, r.next_stage, r.exit), (stage, Some(next), StageExit::Advance));
    assert_eq!((r.entered_at, r.left_at), (Some(entered), left));
    assert_eq!(r.forge_transition_at, Some(left), "the forge instant keys the fact id");
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Forge));
    assert_eq!(r.dwell_sec, Some((left - entered).num_seconds()));
    assert_eq!((r.issue, r.pr_number, r.resolution_sec), (ISSUE, None, Some(0)));
}

fn curated_then_approved_forge() -> Forge {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", minutes(2));
    forge.label(ISSUE, "loom:issue", minutes(7));
    forge
}

#[test]
fn curated_then_ready_gives_one_triage_then_one_approval() {
    let created = t0() - Duration::hours(2);
    let mut forge = curated_then_approved_forge();
    let out = run_curated(
        &[
            ready(&[], Vec::new()),
            ready(&[], Vec::new()),
            ready(&[ISSUE], Vec::new()),
            ready(&[ISSUE], Vec::new()),
        ],
        &[
            curated(&[], created),
            curated(&[ISSUE], created),
            curated(&[], created),
            curated(&[], created),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(out[0].is_empty());
    let triage = pre_ready(&out[1]);
    assert_eq!(triage.len(), 1, "{out:?}");
    assert_exit(
        triage[0],
        FleetStage::TriageWait,
        (created, minutes(2)),
        FleetStage::ApprovalWait,
    );
    // Entering ready adds the approval only: the triage exit is not repeated.
    let approval = pre_ready(&out[2]);
    assert_eq!(approval.len(), 1, "{out:?}");
    assert_exit(
        approval[0],
        FleetStage::ApprovalWait,
        (minutes(2), minutes(7)),
        FleetStage::ReadyWait,
    );
    assert!(pre_ready(&out[3]).is_empty());
}

#[test]
fn an_issue_first_seen_ready_gives_both_exits_and_no_extra_wire_row() {
    let created = t0() - Duration::hours(2);
    let mut forge = curated_then_approved_forge();
    let mut seen = ready(&[ISSUE], Vec::new());
    seen.ready.as_mut().unwrap().items[0].created_at = Some(created);
    let inputs = [ready(&[], Vec::new()), ready(&[], Vec::new()), seen];
    let out = run_curated(
        &inputs,
        &[
            curated(&[], created),
            curated(&[], created),
            curated(&[], created),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let both = pre_ready(&out[2]);
    assert_eq!(both.len(), 2, "{out:?}");
    assert_exit(both[0], FleetStage::TriageWait, (created, minutes(2)), FleetStage::ApprovalWait);
    assert_exit(
        both[1],
        FleetStage::ApprovalWait,
        (minutes(2), minutes(7)),
        FleetStage::ReadyWait,
    );
    let view = build_view(&inputs[2], None, minutes(10));
    let on_wire: Vec<FleetStage> = view.repos[REPO].rows.values().map(|r| r.stage).collect();
    assert_eq!(on_wire, [FleetStage::ReadyWait], "pre-ready stages are never rows");
}

#[test]
fn a_starred_one_step_promotion_has_triage_up_to_the_issue_label_and_no_approval() {
    let created = t0() - Duration::hours(1);
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:issue", minutes(2));
    let mut seen = ready(&[ISSUE], Vec::new());
    seen.ready.as_mut().unwrap().items[0].created_at = Some(created);
    let out = run(&[ready(&[], Vec::new()), seen], Duration::minutes(5), &mut forge);
    let records = pre_ready(&out[1]);
    assert_eq!(records.len(), 1, "{out:?}");
    assert_exit(records[0], FleetStage::TriageWait, (created, minutes(2)), FleetStage::ReadyWait);
}

#[test]
fn a_reopened_issue_is_dated_from_its_latest_reopen_and_latest_curated() {
    let created = t0() - Duration::days(10);
    let mut forge = Forge::default();
    // An earlier cycle that must not be reported again.
    forge.label(ISSUE, "loom:curated", t0() - Duration::days(9));
    forge.label(ISSUE, "loom:issue", t0() - Duration::days(8));
    forge.event(ISSUE, t0() - Duration::days(7), HistoryEvent::Closed);
    forge.event(ISSUE, t0() - Duration::hours(1), HistoryEvent::Reopened);
    forge.label(ISSUE, "loom:curated", minutes(1));
    forge.label(ISSUE, "loom:curated", minutes(2));
    forge.label(ISSUE, "loom:issue", minutes(3));
    let mut seen = ready(&[ISSUE], Vec::new());
    seen.ready.as_mut().unwrap().items[0].created_at = Some(created);
    let out = run(&[ready(&[], Vec::new()), seen], Duration::minutes(5), &mut forge);
    let records = pre_ready(&out[1]);
    assert_eq!(records.len(), 2, "{out:?}");
    assert_exit(
        records[0],
        FleetStage::TriageWait,
        (t0() - Duration::hours(1), minutes(1)),
        FleetStage::ApprovalWait,
    );
    assert_exit(
        records[1],
        FleetStage::ApprovalWait,
        (minutes(2), minutes(3)),
        FleetStage::ReadyWait,
    );
}

#[test]
fn curated_and_never_approved_gives_triage_once_across_passes() {
    let created = t0() - Duration::hours(2);
    let mut forge = curated_then_approved_forge();
    forge.histories.get_mut(&ISSUE).unwrap().events.pop();
    let same = || curated(&[ISSUE], created);
    let inputs: Vec<FleetInput> = (0..4).map(|_| ready(&[], Vec::new())).collect();
    let out = run_curated(
        &inputs,
        &[curated(&[], created), same(), same(), same()],
        Duration::minutes(5),
        &mut forge,
    );
    assert_eq!(pre_ready(&out[1]).len(), 1);
    assert!(out[2..].iter().all(|o| pre_ready(o).is_empty()), "{out:?}");
}

#[test]
fn two_hosts_derive_the_same_exits_whichever_path_showed_the_issue() {
    let created = t0() - Duration::hours(2);
    let by_listing = {
        let mut forge = curated_then_approved_forge();
        run_curated(
            &[
                ready(&[], Vec::new()),
                ready(&[], Vec::new()),
                ready(&[ISSUE], Vec::new()),
            ],
            &[
                curated(&[], created),
                curated(&[ISSUE], created),
                curated(&[], created),
            ],
            Duration::minutes(5),
            &mut forge,
        )
    };
    let by_ready_only = {
        let mut forge = curated_then_approved_forge();
        let mut seen = ready(&[ISSUE], Vec::new());
        seen.ready.as_mut().unwrap().items[0].created_at = Some(created);
        run_curated(
            &[ready(&[], Vec::new()), ready(&[], Vec::new()), seen],
            &[
                curated(&[], created),
                curated(&[], created),
                curated(&[], created),
            ],
            Duration::minutes(5),
            &mut forge,
        )
    };
    let key = |out: &[Vec<TelemetryRecord>]| {
        let mut keys: Vec<_> = out
            .iter()
            .flat_map(|o| pre_ready(o))
            .map(|r| (r.issue, r.stage, r.entered_at, r.forge_transition_at, r.next_stage))
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(key(&by_listing).len(), 2);
    assert_eq!(key(&by_listing), key(&by_ready_only));
}

#[test]
fn a_failed_curated_listing_gives_nothing_until_the_first_whole_one() {
    let created = t0() - Duration::hours(2);
    let mut forge = curated_then_approved_forge();
    forge.histories.get_mut(&ISSUE).unwrap().events.pop();
    let inputs: Vec<FleetInput> = (0..3).map(|_| ready(&[], Vec::new())).collect();
    // Pass 0's listing failed (the repo is absent); pass 1 is the first whole
    // one and shows the issue already curated: its triage exit arrives once.
    let out = run_curated(
        &inputs,
        &[
            Curated::new(),
            curated(&[ISSUE], created),
            curated(&[ISSUE], created),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(pre_ready(&out[0]).is_empty(), "{out:?}");
    let triage = pre_ready(&out[1]);
    assert_eq!(triage.len(), 1, "{out:?}");
    assert_exit(
        triage[0],
        FleetStage::TriageWait,
        (created, minutes(2)),
        FleetStage::ApprovalWait,
    );
    assert!(pre_ready(&out[2]).is_empty(), "{out:?}");
}

/// Curated 90 minutes and approved 60 minutes before the first pass.
fn earlier_forge(approved: bool) -> Forge {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", minutes(-90));
    if approved {
        forge.label(ISSUE, "loom:issue", minutes(-60));
    }
    forge
}

#[test]
fn a_populated_startup_listing_gives_the_triage_exit_on_the_first_pass() {
    let created = t0() - Duration::hours(2);
    let mut forge = earlier_forge(false);
    let inputs: Vec<FleetInput> = (0..2).map(|_| ready(&[], Vec::new())).collect();
    let out = run_curated(
        &inputs,
        &[curated(&[ISSUE], created), curated(&[ISSUE], created)],
        Duration::minutes(5),
        &mut forge,
    );
    let triage = pre_ready(&out[0]);
    assert_eq!(triage.len(), 1, "{out:?}");
    assert_exit(
        triage[0],
        FleetStage::TriageWait,
        (created, minutes(-90)),
        FleetStage::ApprovalWait,
    );
    assert!(pre_ready(&out[1]).is_empty(), "never repeated: {out:?}");
}

#[test]
fn first_pass_ready_rows_give_both_exits() {
    let created = t0() - Duration::hours(2);
    let mut forge = earlier_forge(true);
    let mut seen = ready(&[ISSUE], Vec::new());
    seen.ready.as_mut().unwrap().items[0].created_at = Some(created);
    let out = run_curated(&[seen], &[curated(&[], created)], Duration::minutes(5), &mut forge);
    let both = pre_ready(&out[0]);
    assert_eq!(both.len(), 2, "{out:?}");
    assert_exit(
        both[0],
        FleetStage::TriageWait,
        (created, minutes(-90)),
        FleetStage::ApprovalWait,
    );
    assert_exit(
        both[1],
        FleetStage::ApprovalWait,
        (minutes(-90), minutes(-60)),
        FleetStage::ReadyWait,
    );
    assert!(
        stages(&out[0])
            .iter()
            .all(|r| r.stage != FleetStage::ReadyWait),
        "{out:?}"
    );
}

#[test]
fn missing_history_or_entry_gives_no_fabricated_exit_and_a_failed_read_is_retried() {
    let created = t0() - Duration::hours(2);
    // No `created_at` and no reopen: the triage entry is unknown.
    let mut forge = curated_then_approved_forge();
    let mut no_created = ready(&[ISSUE], Vec::new());
    no_created.ready.as_mut().unwrap().items[0].created_at = None;
    let out = run(&[ready(&[], Vec::new()), no_created], Duration::minutes(5), &mut forge);
    assert!(pre_ready(&out[1]).is_empty(), "{out:?}");
    // A curated label that precedes creation (out of order) is not a triage
    // exit, and an issue label with no curated label before it has no approval.
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", created - Duration::hours(1));
    let out = run_curated(
        &[ready(&[], Vec::new()), ready(&[], Vec::new())],
        &[curated(&[], created), curated(&[ISSUE], created)],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(pre_ready(&out[1]).is_empty(), "{out:?}");
    // A failed history read is retried on the next pass, then emitted once.
    let mut forge = curated_then_approved_forge();
    forge.histories.get_mut(&ISSUE).unwrap().events.pop();
    forge.history_fail_reads = 1;
    let inputs: Vec<FleetInput> = (0..4).map(|_| ready(&[], Vec::new())).collect();
    let listings = [
        curated(&[], created),
        curated(&[ISSUE], created),
        curated(&[ISSUE], created),
        curated(&[ISSUE], created),
    ];
    let out = run_curated(&inputs, &listings, Duration::minutes(5), &mut forge);
    assert!(pre_ready(&out[1]).is_empty(), "{out:?}");
    assert_eq!(pre_ready(&out[2]).len(), 1, "{out:?}");
    assert!(pre_ready(&out[3]).is_empty(), "{out:?}");
}

#[test]
fn a_pending_retry_survives_an_incomplete_curated_listing() {
    let created = t0() - Duration::hours(2);
    let mut forge = curated_then_approved_forge();
    forge.histories.get_mut(&ISSUE).unwrap().events.pop();
    forge.history_fail_reads = 1;
    let inputs: Vec<FleetInput> = (0..4).map(|_| ready(&[], Vec::new())).collect();
    // Complete listing with a failed history read, then a failed listing
    // (the repo is absent), then a complete listing again.
    let listings = [
        curated(&[], created),
        curated(&[ISSUE], created),
        Curated::new(),
        curated(&[ISSUE], created),
    ];
    let out = run_curated(&inputs, &listings, Duration::minutes(5), &mut forge);
    assert!(pre_ready(&out[1]).is_empty(), "{out:?}");
    let total: usize = out.iter().map(|o| pre_ready(o).len()).sum();
    assert_eq!(total, 1, "exactly one triage outcome: {out:?}");
}
