//! `start-v1` and the unstarted `land-v1` path (#9326): the `ready_wait`
//! stage, its history selection, the extended combinator, the ready-queue
//! ingestion and the slot-turnover journal rows.

use super::{as_of, history_a, input_at, provenance, READY_GOLDEN};
use crate::eta::explanation::Explanation;
use crate::eta::heuristics::{FinishV1, LandV1, StartV1, START_V1};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::labels::{ready_row_reason, unstarted_issue_reason};
use crate::eta::score::OutcomeKind;
use crate::eta::simulate::{run_explanation, run_marks};
use crate::eta::tracker::{
    EstimateContext, ItemKey, ReadyPlan, ReadyRow, Tracker, SLOT_TURNOVER_REPO,
};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, DispatchInput, EstimateInput, Heuristic, Kind,
    NoEstimateReason, Registry, Stage, MIN_SAMPLES,
};
use crate::observability::ops::turnaround::Turnover;
use crate::types::{DispatchPlanContext, PlanGate, PlanSlots, PlanState, RowPlan};
use chrono::Duration;
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

/// `n` slot-turnover samples of `base, 2·base, …` seconds, as the tracker
/// journals them (host-wide, stage journal).
fn turnovers(n: usize, base: i64) -> Vec<StageSample> {
    (0..n)
        .map(|i| StageSample {
            repo: SLOT_TURNOVER_REPO.to_string(),
            stage: Stage::ReadyWait,
            duration_sec: base * (i as i64 + 1),
            observed_at: as_of() - Duration::hours(i as i64 + 1),
            source: SampleSource::StageJournal,
            host: "host-fixture-a".to_string(),
            worked: None,
        })
        .collect()
}

/// history-a plus 12 turnovers of 300..=3600 s.
fn history_ready() -> StageSamples {
    let mut history = history_a();
    history.stages.extend(turnovers(12, 300));
    history
}

/// Position 5, four waiting rows ahead, one free slot: four turnovers.
fn dispatch() -> DispatchInput {
    DispatchInput {
        position: 5,
        plan_state: "queued".to_string(),
        gate: Some("capacity".to_string()),
        ahead: 4,
        free_slots: 1,
        max_admissions_per_tick: Some(2),
        tick_interval_secs: 60,
        saturation_held: false,
        plan_at: as_of() - Duration::seconds(30),
    }
}

fn ready_input(dispatch: Option<DispatchInput>) -> EstimateInput {
    let mut input = input_at(Stage::ReadyWait, 600, 0);
    input.subject.pr_number = None;
    input.subject.sweep_id = None;
    input.current = CurrentState::At(CurrentStage {
        stage: Stage::ReadyWait,
        entered_at: Some(as_of() - Duration::seconds(600)),
        age_sec: 600,
        age_source: AgeSource::TrackerObserved,
        rework_rounds: 0,
    });
    input.dispatch = dispatch;
    input
}

#[test]
fn dispatch_input_turnovers_and_admission_delay() {
    let d = dispatch();
    assert_eq!(d.turnovers(), 4);
    assert_eq!(d.admission_delay_sec(), 30, "half a tick once a slot frees");
    // Three free slots, two rows ahead: no turnover; the admission cap of 2
    // puts it in the second batch — one whole tick more.
    let next = DispatchInput {
        ahead: 2,
        free_slots: 3,
        ..dispatch()
    };
    assert_eq!(next.turnovers(), 0);
    assert_eq!(next.admission_delay_sec(), 30 + 60);
    let uncapped = DispatchInput {
        max_admissions_per_tick: None,
        ..next
    };
    assert_eq!(uncapped.admission_delay_sec(), 30);
}

#[test]
fn start_v1_estimates_a_positioned_ready_item() {
    let explanation = StartV1.estimate(&ready_input(Some(dispatch())), &history_ready());
    assert_eq!(explanation.no_estimate_reason, None);
    assert_eq!(explanation.kind, Kind::Start);
    assert_eq!(explanation.heuristic, START_V1);
    // The immutability guard for `start-v1`: these literals may never change.
    assert_eq!(explanation.quantiles(), Some(START_V1_GOLDEN), "start-v1 output moved");
    let path = explanation.path.as_ref().unwrap();
    assert_eq!((path.start, path.terminal), (Stage::ReadyWait, Stage::ReadyWait));
    let record = path.dispatch.as_ref().unwrap();
    assert_eq!((record.turnovers, record.admission_delay_sec), (4, 30));
    let stages: Vec<Stage> = explanation.stages.iter().map(|e| e.stage).collect();
    assert_eq!(stages, vec![Stage::ReadyWait]);
    let entry = &explanation.stages[0];
    assert_eq!(entry.distribution.filters.level, "host", "turnover is host-wide");
    assert!(entry.conditioning.is_none(), "a queue wait is never age-conditioned");
    assert_eq!(entry.mean_visits, Some(4.0));
    assert!(explanation.branches.is_none());
    // Four turnovers of 300..=3600 s plus 30 s: well above one draw's median.
    let (p25, p50, p75) = explanation.quantiles().unwrap();
    assert!(p25 <= p50 && p50 <= p75 && p50 > 4 * 1000);
}

#[test]
fn land_v1_unstarted_prepends_the_queue_wait() {
    let history = history_ready();
    let land = LandV1.estimate(&ready_input(Some(dispatch())), &history);
    assert_eq!(land.no_estimate_reason, None);
    assert_eq!(land.quantiles(), Some(LAND_V1_UNSTARTED_GOLDEN), "land-v1 unstarted moved");
    let stages: Vec<Stage> = land.stages.iter().map(|e| e.stage).collect();
    assert_eq!(
        &stages[..4],
        &[
            Stage::ReadyWait,
            Stage::SweepCurator,
            Stage::SweepBuilder,
            Stage::ReviewWait
        ]
    );
    assert_eq!(land.path.as_ref().unwrap().terminal, Stage::MergeWait);
    let start = StartV1.estimate(&ready_input(Some(dispatch())), &history);
    let started = LandV1.estimate(&input_at(Stage::SweepCurator, 0, 0), &history);
    let (_, start_p50, _) = start.quantiles().unwrap();
    let (_, land_p50, _) = land.quantiles().unwrap();
    let (_, started_p50, _) = started.quantiles().unwrap();
    assert!(
        land_p50 > start_p50 && land_p50 > started_p50,
        "{land_p50} {start_p50} {started_p50}"
    );
    // The started path is untouched by the ready extension.
    assert!(started.path.as_ref().unwrap().dispatch.is_none());
}

#[test]
fn ready_explanations_recompute_from_their_own_json() {
    let history = history_ready();
    for explanation in [
        StartV1.estimate(&ready_input(Some(dispatch())), &history),
        LandV1.estimate(&ready_input(Some(dispatch())), &history),
    ] {
        let json = serde_json::to_string(&explanation).unwrap();
        let parsed: Explanation = serde_json::from_str(&json).unwrap();
        assert_eq!(run_explanation(&parsed), explanation.quantiles_with_p90());
        let (p25, p50, p75, p90) = explanation.quantiles_with_p90().expect("p90 recorded");
        assert!(p25 <= p50 && p50 <= p75 && p75 <= p90, "{}", explanation.heuristic);
        let result = explanation.result.as_ref().unwrap();
        let marks = run_marks(&parsed).unwrap();
        assert_eq!(marks, result.stage_marks);
        // The ready_wait mark leads and is the origin; the terminal mark is
        // the estimate.
        assert_eq!(marks[0].stage, Stage::ReadyWait);
        if explanation.kind == Kind::Land {
            assert_eq!(marks[0].p50_at, Some(explanation.as_of));
        }
        let terminal = explanation.path.as_ref().unwrap().terminal;
        let mark = marks.iter().find(|m| m.stage == terminal).unwrap();
        assert_eq!(mark.p50_at, Some(result.eta_p50_at));
        assert_eq!(marks.len(), 6, "ready_wait plus the five post-dispatch stages");
    }
}

#[test]
fn ready_golden_fixture() {
    let history = history_ready();
    let actual = serde_json::json!({
        "start": StartV1.estimate(&ready_input(Some(dispatch())), &history),
        "land": LandV1.estimate(&ready_input(Some(dispatch())), &history),
    });
    if std::env::var("LOOM_ETA_BLESS").is_ok_and(|v| v == "1") {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/eta/fixtures/ready-golden.json");
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
    let golden: serde_json::Value = serde_json::from_str(READY_GOLDEN).unwrap();
    assert!(
        actual == golden,
        "start-v1 / unstarted land-v1 drifted from fixtures/ready-golden.json \
         (a shipped heuristic id is immutable)"
    );
    for kind in ["start", "land"] {
        let parsed: Explanation = serde_json::from_value(golden[kind].clone()).unwrap();
        assert!(parsed.quantiles_with_p90().is_some(), "{kind} records p90");
        assert_eq!(run_explanation(&parsed), parsed.quantiles_with_p90(), "{kind} recomputes");
    }
}

const START_V1_GOLDEN: (i64, i64, i64) = (6307, 7723, 9162);
const LAND_V1_UNSTARTED_GOLDEN: (i64, i64, i64) = (11623, 13930, 16310);

#[test]
fn too_few_turnovers_is_insufficient_samples_never_a_number() {
    let mut history = history_a();
    history.stages.extend(turnovers(MIN_SAMPLES - 1, 300));
    // Whatever the position — even one needing no turnover at all.
    let free = DispatchInput {
        ahead: 0,
        free_slots: 2,
        ..dispatch()
    };
    for d in [dispatch(), free] {
        for explanation in [
            StartV1.estimate(&ready_input(Some(d.clone())), &history),
            LandV1.estimate(&ready_input(Some(d.clone())), &history),
        ] {
            assert_eq!(
                explanation.no_estimate_reason,
                Some(NoEstimateReason::InsufficientSamples),
                "{}",
                explanation.kind
            );
            assert_eq!(explanation.result, None);
        }
    }
    // A turnover the tracker saw while in the queue is not a sample: the
    // `ready_wait` grid reads the stage journal only.
    let mut outcome_only = history_a();
    for mut s in turnovers(12, 300) {
        s.source = SampleSource::SweepOutcome;
        outcome_only.stages.push(s);
    }
    let explanation = StartV1.estimate(&ready_input(Some(dispatch())), &outcome_only);
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
}

#[test]
fn no_position_is_no_dispatch_plan_and_kinds_stay_in_their_lanes() {
    let history = history_ready();
    for explanation in [
        StartV1.estimate(&ready_input(None), &history),
        LandV1.estimate(&ready_input(None), &history),
    ] {
        assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::NoDispatchPlan));
        assert_eq!(explanation.loom, provenance(), "a refusal still names its build");
    }
    // `start` is only for ready items; `finish` never is.
    let started = StartV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    assert_eq!(started.no_estimate_reason, Some(NoEstimateReason::UnknownStage));
    let finish = FinishV1.estimate(&ready_input(Some(dispatch())), &history);
    assert_eq!(finish.no_estimate_reason, Some(NoEstimateReason::UnknownStage));
}

#[test]
fn registry_serves_start_v1_as_current() {
    let registry = Registry::builtin();
    assert_eq!(registry.current(Kind::Start, None).id(), START_V1);
    assert_eq!(registry.current(Kind::Start, Some("land-v1")).id(), START_V1, "kind must match");
    assert!(registry.ids().contains(&START_V1));
    assert_eq!(Kind::Start.as_str(), "start");
    assert_eq!(serde_json::to_value(Stage::ReadyWait).unwrap(), "ready_wait");
    assert!(!Stage::ALL.contains(&Stage::ReadyWait), "post-dispatch list is unchanged");
}

#[test]
fn ready_rows_narrow_the_refusal() {
    let plan = |state, position| RowPlan {
        plan_state: state,
        position,
        ..RowPlan::default()
    };
    assert_eq!(ready_row_reason(&plan(PlanState::Next, Some(1))), None);
    assert_eq!(ready_row_reason(&plan(PlanState::Queued, Some(9))), None);
    for (state, position) in [
        (PlanState::Blocked, None),
        (PlanState::Queued, None),
        (PlanState::Unknown, Some(3)),
    ] {
        assert_eq!(
            ready_row_reason(&plan(state, position)),
            Some(NoEstimateReason::NoDispatchPlan)
        );
    }
    let ready = vec!["loom:issue".to_string()];
    assert_eq!(unstarted_issue_reason(&ready, Some(&plan(PlanState::Next, Some(1)))), None);
    assert_eq!(unstarted_issue_reason(&ready, None), Some(NoEstimateReason::NoDispatchPlan));
    let held = vec!["loom:issue".to_string(), "loom:blocked".to_string()];
    assert_eq!(
        unstarted_issue_reason(&held, Some(&plan(PlanState::Next, Some(1)))),
        Some(NoEstimateReason::Blocked),
        "a hold label outranks the plan"
    );
}

// ---- the tracker ----

fn row(issue: u32, state: PlanState, position: Option<u32>) -> ReadyRow {
    ReadyRow {
        repo: REPO.to_string(),
        issue,
        plan: RowPlan {
            plan_state: state,
            position,
            gate: position.map(|_| PlanGate::Capacity),
            ..RowPlan::default()
        },
        disposition: crate::types::QueueDisposition::DeferredCapacity,
    }
}

fn ready_plan(complete: bool) -> ReadyPlan {
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
            complete,
            ..DispatchPlanContext::default()
        },
        at: as_of() - Duration::seconds(10),
        listing_failed: Vec::new(),
    }
}

fn estimate(tracker: &mut Tracker, history: &StageSamples) -> Vec<crate::eta::tracker::Emission> {
    let registry = Registry::builtin();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
    };
    tracker.estimate(None, &ctx, as_of())
}

#[test]
fn ready_queue_ingestion_estimates_start_and_land_then_settles_on_dispatch() {
    let history = history_ready();
    let mut tracker = Tracker::new(provenance());
    let rows = [
        row(10, PlanState::Next, Some(1)),
        row(11, PlanState::Queued, Some(2)),
        row(12, PlanState::Blocked, None),
        row(13, PlanState::Running, None),
    ];
    let effects = tracker.on_ready_queue(&rows, &ready_plan(true), as_of());
    assert_eq!(effects.journal.len(), 3, "first sight of each non-running row");
    assert!(effects.journal.iter().all(|r| r.event == "ready.first_seen"
        && r.next_stage == Some(Stage::ReadyWait)
        && r.duration_sec.is_none()));
    assert!(
        !tracker.item_keys().contains(&ItemKey::new(REPO, 13)),
        "running rows are the sweep's"
    );

    let emissions = estimate(&mut tracker, &history);
    let by = |issue: u32, kind: Kind| {
        emissions
            .iter()
            .find(|e| e.explanation.subject.issue == issue && e.explanation.kind == kind)
            .map(|e| &e.explanation)
            .unwrap_or_else(|| panic!("{issue} {kind}"))
    };
    for issue in [10, 11] {
        for kind in [Kind::Start, Kind::Land] {
            let e = by(issue, kind);
            assert!(e.quantiles().is_some(), "{issue} {kind}: {:?}", e.no_estimate_reason);
            assert!(e.loom.is_valid() && e.loom == provenance(), "full provenance");
            assert_eq!(e.features.as_ref().unwrap().queue_ready, Some(4));
        }
        assert!(!emissions
            .iter()
            .any(|e| e.explanation.subject.issue == issue && e.explanation.kind == Kind::Finish));
    }
    let d10 = by(10, Kind::Start)
        .path
        .as_ref()
        .unwrap()
        .dispatch
        .clone()
        .unwrap();
    let d11 = by(11, Kind::Start)
        .path
        .as_ref()
        .unwrap()
        .dispatch
        .clone()
        .unwrap();
    assert_eq!((d10.input.ahead, d10.turnovers), (0, 1));
    assert_eq!((d11.input.ahead, d11.turnovers), (1, 2));
    for kind in [Kind::Start, Kind::Land] {
        assert_eq!(by(12, kind).no_estimate_reason, Some(NoEstimateReason::NoDispatchPlan));
    }

    // The dispatch settles every `start` estimate of the item and hands it to
    // the sweep's chain: no more `start`, no gap in `finish`/`land`.
    let dispatched =
        tracker.on_dispatch(REPO, 10, "sweep-issue-10-1", as_of() + Duration::seconds(900));
    assert_eq!(dispatched.outcomes.len(), 1);
    let outcome = &dispatched.outcomes[0];
    assert_eq!(outcome.estimate.kind, Kind::Start);
    assert_eq!(outcome.score.outcome, OutcomeKind::Started);
    assert_eq!(outcome.score.lead_sec, 900);
    assert!(outcome.score.error_sec.is_some(), "a start is scored");
    let row = &dispatched.journal[0];
    assert_eq!(row.stage, Some(Stage::ReadyWait));
    assert_eq!(row.duration_sec, None, "a whole queue wait is never a turnover sample");
    let after = estimate(&mut tracker, &history);
    let kinds: Vec<Kind> = after
        .iter()
        .filter(|e| e.explanation.subject.issue == 10 && e.primary)
        .map(|e| e.explanation.kind)
        .collect();
    assert_eq!(kinds, vec![Kind::Finish, Kind::Land]);
    // The `land-v2` shadow rides alongside, never as the primary (#9328).
    assert!(after.iter().any(|e| e.explanation.subject.issue == 10
        && !e.primary
        && e.explanation.heuristic == crate::eta::heuristics::LAND_V2));
    assert!(tracker
        .pending()
        .iter()
        .all(|p| !(p.issue == 10 && p.kind == Kind::Start)));
}

#[test]
fn a_ready_item_leaving_a_complete_plan_queues_one_issue_read() {
    let mut tracker = Tracker::new(provenance());
    tracker.on_ready_queue(&[row(10, PlanState::Next, Some(1))], &ready_plan(true), as_of());
    // An incomplete plan says nothing about the rows it lacks.
    tracker.on_ready_queue(&[], &ready_plan(false), as_of());
    let listing = tracker.on_listing(REPO, &[], as_of(), 300);
    assert!(listing.issue_checks.is_empty());
    // A complete one does.
    tracker.on_ready_queue(&[], &ready_plan(true), as_of());
    let listing = tracker.on_listing(REPO, &[], as_of(), 300);
    assert_eq!(listing.issue_checks, vec![ItemKey::new(REPO, 10)]);
}

#[test]
fn a_stale_plan_refuses_with_stale_inputs() {
    let history = history_ready();
    let mut tracker = Tracker::new(provenance());
    let mut plan = ready_plan(true);
    plan.at = as_of() - Duration::hours(2);
    tracker.on_ready_queue(&[row(10, PlanState::Next, Some(1))], &plan, as_of());
    let emissions = estimate(&mut tracker, &history);
    assert!(!emissions.is_empty());
    for e in &emissions {
        assert_eq!(e.explanation.no_estimate_reason, Some(NoEstimateReason::StaleInputs));
    }
}

#[test]
fn slot_turnover_rows_feed_the_ready_wait_history() {
    let tracker = Tracker::new(provenance());
    let from = as_of() - Duration::seconds(1000);
    let effects = tracker.on_slot_turnover(&Turnover {
        from,
        to: as_of() - Duration::seconds(100),
        running_at_start: 3,
    });
    let row = &effects.journal[0];
    assert_eq!(row.event, "slot.turnover");
    assert_eq!(row.stage, Some(Stage::ReadyWait));
    assert_eq!(row.duration_sec, Some(900));
    assert!(!row.in_sweep);
    assert_eq!(row.loom, provenance());
    // Round trip through the journal into history.
    let json = serde_json::to_string(row).unwrap();
    let parsed: crate::eta::journal::JournalEntry = serde_json::from_str(&json).unwrap();
    let mut history = StageSamples::default();
    history.push_journal(&vec![parsed; MIN_SAMPLES], "host-a");
    let selection = history
        .select(REPO, Stage::ReadyWait, as_of(), &[SampleSource::StageJournal])
        .expect("host-level selection");
    assert_eq!(selection.level.as_str(), "host");
    assert_eq!(selection.sorted, vec![900; MIN_SAMPLES]);
    // Leak-free like every other stage: not visible before it was observed.
    assert!(history
        .select(REPO, Stage::ReadyWait, from, &[SampleSource::StageJournal])
        .is_none());
}

#[test]
fn start_v1_is_backtestable_from_the_tracker_journal() {
    use crate::eta::backtest::{self, cases_from_journal, Filter};
    let mut tracker = Tracker::new(provenance());
    let mut journal = tracker
        .on_ready_queue(
            &[
                row(10, PlanState::Next, Some(1)),
                row(12, PlanState::Blocked, None),
            ],
            &ready_plan(true),
            as_of(),
        )
        .journal;
    journal.extend(
        tracker
            .on_dispatch(REPO, 10, "sweep-issue-10-1", as_of() + Duration::seconds(900))
            .journal,
    );
    // The blocked row was never positioned and never dispatched: no case.
    journal.extend(
        tracker
            .on_dispatch(REPO, 12, "sweep-issue-12-1", as_of() + Duration::seconds(950))
            .journal,
    );
    // Through the journal's own serialisation, as `eta backtest` reads it.
    let journal: Vec<crate::eta::journal::JournalEntry> = journal
        .iter()
        .map(|e| serde_json::from_str(&serde_json::to_string(e).unwrap()).unwrap())
        .collect();
    let cases = cases_from_journal(&journal);
    assert_eq!(cases.len(), 1, "{cases:?}");
    let case = &cases[0];
    assert_eq!((case.subject.issue, case.kind, case.stage), (10, Kind::Start, Stage::ReadyWait));
    assert_eq!((case.as_of, case.actual_at), (as_of(), as_of() + Duration::seconds(900)));
    assert_eq!(case.dispatch.as_ref().map(|d| (d.position, d.ahead)), Some((1, 0)));

    let filter = Filter {
        since: None,
        repo: None,
    };
    let report = backtest::run(&StartV1, &history_ready(), &cases, filter, &provenance());
    assert_eq!(report.kind, Kind::Start);
    assert_eq!((report.overall.n, report.overall.scored), (1, 1));
    // Without its turnover history, the same case is refused, not invented.
    let report = backtest::run(&StartV1, &history_a(), &cases, filter, &provenance());
    assert_eq!((report.overall.n, report.overall.refused), (1, 1));
}
