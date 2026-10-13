//! Stays a close ends in `ready_wait`, `triage_wait` and `approval_wait`, and
//! stays a repo's removal from the fleet ends (#11398), beside the shared
//! helpers of `outcomes_tests`.

use super::close::REPO_REMOVAL_CONFIRM_PASSES;
use super::*;

const OTHER: &str = "rjwalters/other";

fn minutes(m: i64) -> DateTime<Utc> {
    t0() + Duration::minutes(m)
}

/// The stage outcomes of `stage` in `records`.
fn of(records: &[TelemetryRecord], stage: FleetStage) -> Vec<StageOutcomeRecord> {
    stages(records)
        .into_iter()
        .filter(|r| r.stage == stage)
        .cloned()
        .collect()
}

/// Asserts `out` holds exactly one record of `stage`, in pass `at`.
fn only_one(out: &[Vec<TelemetryRecord>], stage: FleetStage, at: usize) -> StageOutcomeRecord {
    for (i, pass) in out.iter().enumerate() {
        let n = of(pass, stage).len();
        assert_eq!(n, usize::from(i == at), "pass {i}: {out:?}");
    }
    of(&out[at], stage).remove(0)
}

/// What the fact id keys on, so two hosts can be compared.
fn key(r: &StageOutcomeRecord) -> impl PartialEq + std::fmt::Debug {
    (r.stage, r.next_stage, r.exit, r.forge_transition_at, r.left_at, r.dwell_sec)
}

// ---- ready_wait ----

/// `ISSUE` ready since `t0 - 30m`, closed at `t0 + 3m` as `facts` says.
fn ready_closed(facts: Option<fn(DateTime<Utc>) -> IssueFacts>) -> Forge {
    let mut forge = ready_forge();
    forge.event(ISSUE, minutes(3), HistoryEvent::Closed);
    if let Some(facts) = facts {
        forge.issues.insert(ISSUE, facts(minutes(3)));
    }
    forge
}

fn ready_close_run(forge: &mut Forge, step: i64) -> Vec<Vec<TelemetryRecord>> {
    let gone = ready(&[], Vec::new());
    run(
        &[ready(&[ISSUE], Vec::new()), gone.clone(), gone],
        Duration::minutes(step),
        forge,
    )
}

#[test]
fn a_closed_ready_issue_takes_the_issue_close_classification() {
    let cases = [
        (IssueFacts::NotMerged as fn(_) -> _, StageExit::Abandoned),
        (IssueFacts::Merged, StageExit::Landed),
    ];
    for (facts, exit) in cases {
        let mut forge = ready_closed(Some(facts));
        let r = only_one(&ready_close_run(&mut forge, 5), FleetStage::ReadyWait, 1);
        assert_eq!((r.exit, r.next_stage), (exit, None));
        assert_eq!((r.forge_transition_at, r.left_at), (Some(minutes(3)), minutes(3)));
        assert_eq!(r.entered_at, Some(t0() - Duration::minutes(30)));
        assert_eq!(r.dwell_sec, Some(33 * 60));
        assert_eq!(r.resolution_sec, Some(0));
        assert!(r.has_fact_key());
        // Another host, polling on another cadence, keys the same fact.
        let mut other = ready_closed(Some(facts));
        let theirs = only_one(&ready_close_run(&mut other, 7), FleetStage::ReadyWait, 1);
        assert_eq!(key(&r), key(&theirs));
    }
}

#[test]
fn a_ready_close_with_no_known_closer_is_unknown_at_the_close() {
    let mut forge = ready_closed(Some(IssueFacts::ClosedUnresolved));
    let r = only_one(&ready_close_run(&mut forge, 5), FleetStage::ReadyWait, 1);
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::Unknown, Some(minutes(3))));
    assert_eq!((r.left_at, r.dwell_sec), (minutes(3), None));
    assert!(r.has_fact_key(), "keyed on the forge close");
}

#[test]
fn a_ready_close_whose_issue_read_fails_waits_then_emits_once() {
    let mut forge = ready_closed(Some(IssueFacts::NotMerged));
    forge.issue_failures = 1;
    let r = only_one(&ready_close_run(&mut forge, 5), FleetStage::ReadyWait, 2);
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::Abandoned, Some(minutes(3))));
    assert_eq!(r.dwell_sec, Some(33 * 60));
}

#[test]
fn a_claimed_ready_issue_still_advances_without_an_issue_read() {
    let mut forge = ready_forge();
    claim(&mut forge, minutes(3));
    let out = run(&[ready(&[ISSUE], Vec::new()), claimed()], Duration::minutes(5), &mut forge);
    assert_claimed_at(&only_one(&out, FleetStage::ReadyWait, 1), minutes(3));
    assert_eq!(forge.issue_reads, 0);
}

// ---- triage_wait / approval_wait ----

/// [`run_curated`] with pass `i` also reading `triage[i]`.
fn run_listed(
    curated: &[Curated],
    triage: &[Curated],
    step: Duration,
    forge: &mut Forge,
) -> Vec<Vec<TelemetryRecord>> {
    let mut memory = Memory::default();
    let mut prev_view = None;
    let mut out = Vec::new();
    for i in 0..curated.len().max(triage.len()) {
        let input = ready(&[], Vec::new());
        let now = t0() + step * i32::try_from(i).unwrap();
        let view = build_view(&input, prev_view.as_ref(), now);
        let listed = listed(&input.listings);
        let pass = Pass {
            view: &view,
            listed: &listed,
            managed: &input.managed,
            now,
        };
        forge.clock = Some(now);
        memory.curated_now = Some(curated.get(i).cloned().unwrap_or_default());
        memory.triage_now = Some(triage.get(i).cloned().unwrap_or_default());
        out.push(diff(&mut memory, &pass, forge, &provenance()));
        memory = remember(memory, &pass);
        prev_view = Some(view);
    }
    out
}

fn listing(issues: &[u32]) -> Curated {
    let created = t0() - Duration::hours(2);
    [(REPO.to_string(), issues.iter().map(|n| (*n, Some(created))).collect())].into()
}

/// Listed in passes 0 and 1, gone from pass 2 on (of 4).
fn leaving() -> Vec<Curated> {
    vec![
        listing(&[ISSUE]),
        listing(&[ISSUE]),
        listing(&[]),
        listing(&[]),
    ]
}

/// `ISSUE` curated at `t0 - 1h`, closed at `t0 + 7m` as `facts` says.
fn curated_closed(facts: fn(DateTime<Utc>) -> IssueFacts) -> Forge {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", t0() - Duration::hours(1));
    forge.event(ISSUE, minutes(7), HistoryEvent::Closed);
    forge.issues.insert(ISSUE, facts(minutes(7)));
    forge
}

#[test]
fn a_curated_issue_closed_before_approval_ends_its_approval_wait_once() {
    let cases = [
        (IssueFacts::NotMerged as fn(_) -> _, StageExit::Abandoned),
        (IssueFacts::Merged, StageExit::Landed),
    ];
    for (facts, exit) in cases {
        let host = |step: i64| {
            let mut forge = curated_closed(facts);
            let out = run_listed(&leaving(), &[], Duration::minutes(step), &mut forge);
            only_one(&out, FleetStage::ApprovalWait, 2)
        };
        let r = host(5);
        assert_eq!((r.exit, r.next_stage), (exit, None));
        assert_eq!((r.forge_transition_at, r.left_at), (Some(minutes(7)), minutes(7)));
        assert_eq!(r.entered_at, Some(t0() - Duration::hours(1)));
        assert_eq!(r.entered_at_source, Some(EnteredAtSource::Forge));
        assert_eq!(r.dwell_sec, Some(67 * 60));
        assert_eq!((r.issue, r.pr_number, r.resolution_sec), (ISSUE, None, Some(0)));
        assert!(r.has_fact_key());
        assert_eq!(key(&r), key(&host(6)), "every host keys the same fact");
    }
}

#[test]
fn a_triage_issue_closed_before_curation_ends_its_triage_wait_once() {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:triage", t0() - Duration::hours(2));
    forge.event(ISSUE, minutes(7), HistoryEvent::Closed);
    forge
        .issues
        .insert(ISSUE, IssueFacts::NotMerged(minutes(7)));
    let out = run_listed(&[], &leaving(), Duration::minutes(5), &mut forge);
    let r = only_one(&out, FleetStage::TriageWait, 2);
    assert_eq!((r.exit, r.next_stage), (StageExit::Abandoned, None));
    assert_eq!((r.forge_transition_at, r.left_at), (Some(minutes(7)), minutes(7)));
    assert_eq!(r.entered_at, Some(t0() - Duration::hours(2)), "its creation");
    assert_eq!(r.dwell_sec, Some(127 * 60));
    assert!(r.has_fact_key());
}

#[test]
fn a_relabelled_pre_ready_issue_is_no_exit() {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", t0() - Duration::hours(1));
    let out = run_listed(&leaving(), &[], Duration::minutes(5), &mut forge);
    assert!(out.iter().skip(1).all(|pass| stages(pass).is_empty()), "{out:?}");
    assert_eq!(forge.issue_reads, 1, "one read, then forgotten");
}

#[test]
fn a_move_from_triage_to_curated_reads_nothing() {
    let mut forge = Forge::default();
    let curated = [listing(&[]), listing(&[ISSUE]), listing(&[ISSUE])];
    let triage = [listing(&[ISSUE]), listing(&[]), listing(&[])];
    run_listed(&curated, &triage, Duration::minutes(5), &mut forge);
    assert_eq!(forge.issue_reads, 0);
}

#[test]
fn an_incomplete_pre_ready_listing_is_not_a_departure() {
    let mut forge = curated_closed(IssueFacts::NotMerged);
    // Pass 2 has no complete listing of the repo.
    let curated = [
        listing(&[ISSUE]),
        listing(&[ISSUE]),
        Curated::new(),
        listing(&[ISSUE]),
    ];
    let out = run_listed(&curated, &[], Duration::minutes(5), &mut forge);
    assert!(
        out.iter()
            .all(|pass| of(pass, FleetStage::ApprovalWait).is_empty()),
        "{out:?}"
    );
    assert_eq!(forge.issue_reads, 0);
}

#[test]
fn a_pre_ready_close_whose_read_fails_is_retried_then_emitted_once() {
    let mut forge = curated_closed(IssueFacts::NotMerged);
    forge.issue_failures = 1;
    let out = run_listed(&leaving(), &[], Duration::minutes(5), &mut forge);
    let r = only_one(&out, FleetStage::ApprovalWait, 3);
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::Abandoned, Some(minutes(7))));
}

// ---- repo removal ----

/// A pass with `REPO` managed (or not) beside `OTHER`, holding `held`.
fn fleet(managed: bool, held: Vec<HeldSweep>) -> FleetInput {
    let mut pass = input(&[], held);
    pass.managed = [OTHER.to_string()].into();
    if managed {
        pass.managed.insert(REPO.to_string());
    } else {
        pass.listings.clear();
    }
    pass
}

fn held_sweep() -> HeldSweep {
    HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::SweepBuilder,
        entered_at: t0() - Duration::minutes(2),
        entered_at_lower_bound: false,
        pr: None,
        overflow: false,
        start: Default::default(),
        sweep_id: Some("sweep-issue-20-1".to_string()),
    }
}

/// `REPO` managed with a builder row in pass 0, then removed for `gone` passes.
fn removed(gone: usize) -> Vec<FleetInput> {
    let mut inputs = vec![fleet(true, vec![held_sweep()])];
    inputs.extend((0..gone).map(|_| fleet(false, Vec::new())));
    inputs
}

const CONFIRM: usize = REPO_REMOVAL_CONFIRM_PASSES as usize;

#[test]
fn a_removed_repo_ends_its_stay_once_at_the_issue_close() {
    let mut forge = Forge::default();
    forge.issues.insert(20, IssueFacts::NotMerged(minutes(7)));
    let out = run(&removed(CONFIRM + 3), Duration::minutes(10), &mut forge);
    let r = only_one(&out, FleetStage::SweepBuilder, CONFIRM);
    assert_eq!((r.exit, r.next_stage), (StageExit::Abandoned, None));
    assert_eq!((r.forge_transition_at, r.left_at), (Some(minutes(7)), minutes(7)));
    assert_eq!(r.dwell_sec, Some(9 * 60));
    assert_eq!(forge.issue_reads, 1);
}

#[test]
fn a_removed_repo_with_no_readable_close_ends_unknown_at_the_removal() {
    for failures in [0, usize::MAX] {
        let mut forge = Forge {
            issue_failures: failures,
            ..Forge::default()
        };
        let out = run(&removed(CONFIRM + 3), Duration::minutes(10), &mut forge);
        let r = only_one(&out, FleetStage::SweepBuilder, CONFIRM);
        assert_eq!((r.exit, r.forge_transition_at), (StageExit::Unknown, None));
        assert_eq!((r.left_at, r.dwell_sec), (minutes(10), None), "the first pass gone");
        assert_eq!(r.fact_id_missing_reason, Some(FactIdMissingReason::NoForgeInstant));
        assert!(r.has_fact_key(), "keyed on its sweep id");
    }
}

#[test]
fn a_repo_removed_while_an_issue_read_is_pending_ends_that_stay_once() {
    for (failures, exit) in [(1, StageExit::Abandoned), (usize::MAX, StageExit::Unknown)] {
        let mut forge = Forge {
            issue_failures: failures,
            ..Forge::default()
        };
        forge.issues.insert(20, IssueFacts::NotMerged(minutes(7)));
        // Pass 1: the row vanishes, its read fails. Pass 2 on: removed.
        let mut inputs = vec![fleet(true, vec![held_sweep()]), fleet(true, Vec::new())];
        inputs.extend((0..CONFIRM + 3).map(|_| fleet(false, Vec::new())));
        let out = run(&inputs, Duration::minutes(10), &mut forge);
        let r = only_one(&out, FleetStage::SweepBuilder, 1 + CONFIRM);
        assert_eq!(r.exit, exit);
        if exit == StageExit::Unknown {
            assert_eq!((r.left_at, r.forge_transition_at), (minutes(10), None), "when it vanished");
        } else {
            assert_eq!(r.forge_transition_at, Some(minutes(7)));
        }
    }
}

#[test]
fn a_repo_out_of_the_fleet_briefly_is_no_removal() {
    let mut forge = Forge::default();
    let pr_row = |managed| {
        let mut pass = fleet(managed, Vec::new());
        if managed {
            pass.listings[0].prs = input(&[(21, "loom:review-requested", 20)], Vec::new())
                .listings
                .remove(0)
                .prs;
        }
        pass
    };
    let mut inputs = vec![pr_row(true)];
    inputs.extend((1..CONFIRM).map(|_| pr_row(false)));
    inputs.extend((0..CONFIRM + 2).map(|_| pr_row(true)));
    let out = run(&inputs, Duration::minutes(10), &mut forge);
    assert!(out.iter().all(|pass| stages(pass).is_empty()), "{out:?}");
    assert_eq!(forge.issue_reads, 0);
}

#[test]
fn an_empty_managed_set_is_never_a_removal() {
    let mut forge = Forge::default();
    let mut inputs = removed(CONFIRM + 3);
    for pass in inputs.iter_mut().skip(1) {
        pass.managed.clear();
    }
    let out = run(&inputs, Duration::minutes(10), &mut forge);
    assert!(out.iter().all(|pass| stages(pass).is_empty()), "{out:?}");
}

#[test]
fn a_removed_repo_ends_its_curated_stays_too() {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:curated", t0() - Duration::hours(1));
    let mut memory = Memory::default();
    let mut prev_view = None;
    let mut out = Vec::new();
    for i in 0..CONFIRM + 3 {
        let input = fleet(i < 2, Vec::new());
        let now = minutes(10 * i64::try_from(i).unwrap());
        let view = build_view(&input, prev_view.as_ref(), now);
        let listed = listed(&input.listings);
        let pass = Pass {
            view: &view,
            listed: &listed,
            managed: &input.managed,
            now,
        };
        forge.clock = Some(now);
        memory.curated_now = Some(if i < 2 {
            listing(&[ISSUE])
        } else {
            Curated::new()
        });
        out.push(diff(&mut memory, &pass, &mut forge, &provenance()));
        memory = remember(memory, &pass);
        prev_view = Some(view);
    }
    let r = only_one(&out, FleetStage::ApprovalWait, 1 + CONFIRM);
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::Unknown, None));
    assert_eq!(r.left_at, minutes(20), "the first pass gone");
    assert_eq!(r.entered_at, Some(t0() - Duration::hours(1)));
    assert!(r.has_fact_key(), "keyed on the forge-dated entry");
}
