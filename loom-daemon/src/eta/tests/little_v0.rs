//! `little-v0` (#10208): the zero-parameter queue floor.

use super::{as_of, input_at, provenance, subject};
use crate::eta::backtest::{self, Filter, ReplayCase};
use crate::eta::heuristics::{wait_sec, LandV2, LittleV0, LITTLE_V0};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::queue_features::{EventKind, EventLog, RosterEntry, StageEvent};
use crate::eta::score::OutcomeKind;
use crate::eta::stage_queue::{
    stage_queue, weighted_rate_per_hr, QueueScope, StageQueue, HALF_LIFE_SEC, WINDOW_SEC,
};
use crate::eta::stall::{StallCause, StallSignal};
use crate::eta::{CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason, Registry, Stage};
use chrono::{DateTime, Duration, Utc};

fn history() -> StageSamples {
    let mut h = StageSamples::default();
    for i in 0..10_i64 {
        h.stages.push(StageSample {
            repo: "rjwalters/loom".to_string(),
            stage: Stage::MergeWait,
            duration_sec: 600 + i * 60,
            observed_at: as_of() - Duration::hours(i + 1),
            source: SampleSource::SweepOutcome,
            host: "host-a".to_string(),
            worked: None,
        });
    }
    h
}

fn queue(stage: Stage, items_ahead: u32, rate: f64, exits: u32) -> StageQueue {
    StageQueue {
        stage,
        scope: QueueScope::of(stage),
        items_ahead,
        drain_rate_per_hr: rate,
        half_life_sec: HALF_LIFE_SEC,
        window_sec: WINDOW_SEC,
        exits,
    }
}

fn input(items_ahead: u32, rate: f64, exits: u32) -> EstimateInput {
    let mut i = input_at(Stage::ReviewWait, 0, 0);
    i.queue = vec![queue(Stage::ReviewWait, items_ahead, rate, exits)];
    i
}

#[test]
fn estimate_is_deterministic_and_rederivable_from_its_explanation() {
    let a = LittleV0.estimate(&input(6, 2.0, 30), &history());
    let b = LittleV0.estimate(&input(6, 2.0, 30), &history());
    assert_eq!(a, b);
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    let q = a.queue.as_ref().expect("queue record");
    assert_eq!((q.items_ahead, q.drain_rate_per_hr, q.exits), (6, 2.0, 30));
    assert_eq!((q.half_life_sec, q.window_sec), (HALF_LIFE_SEC, WINDOW_SEC));
    // The point estimate from the explanation alone.
    let service: f64 = q.service.iter().map(|s| s.mean_sec).sum();
    let p50 = wait_sec(q.items_ahead, q.drain_rate_per_hr) + service.round() as i64;
    let result = a.result.as_ref().unwrap();
    assert_eq!(result.p50_sec, p50);
    assert_eq!(q.wait_sec, 3 * 3600);
    assert!(result.p25_sec <= result.p50_sec && result.p50_sec <= result.p75_sec);
    assert!(result.p75_sec <= result.p90_sec.unwrap());
    assert!(result.p25_sec < result.p75_sec, "a queue wait has an interval");
}

#[test]
fn zero_queue_waits_only_the_service_time() {
    let e = LittleV0.estimate(&input(0, 0.0, 0), &history());
    let r = e.result.as_ref().expect("zero queue needs no drain rate");
    let q = e.queue.as_ref().unwrap();
    assert_eq!(q.wait_sec, 0);
    assert_eq!(r.p50_sec, q.service_total_sec);
    assert_eq!((r.p25_sec, r.p75_sec), (r.p50_sec, r.p50_sec));
    assert!(r.p50_sec > 0);
}

#[test]
fn zero_drain_and_missing_context_refuse_without_panic() {
    let e = LittleV0.estimate(&input(3, 0.0, 0), &history());
    assert!(e.result.is_none());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    let none = LittleV0.estimate(&input_at(Stage::ReviewWait, 0, 0), &history());
    assert!(none.result.is_none() && none.no_estimate_reason.is_some());
    // No service history for the later stage.
    let bare = LittleV0.estimate(&input(3, 1.0, 5), &StageSamples::default());
    assert!(bare.result.is_none());
    // A pre-PR stage has no queue to read.
    let mut pre = input_at(Stage::SweepBuilder, 0, 0);
    pre.queue = vec![queue(Stage::SweepBuilder, 1, 1.0, 5)];
    assert_eq!(
        LittleV0.estimate(&pre, &history()).no_estimate_reason,
        Some(NoEstimateReason::UnknownStage)
    );
}

#[test]
fn interval_widens_with_fewer_observed_exits() {
    let width = |exits: u32| {
        let r = LittleV0
            .estimate(&input(6, 2.0, exits), &history())
            .result
            .unwrap();
        r.p75_sec - r.p25_sec
    };
    let (many, few) = (width(200), width(3));
    assert!(few > many, "few exits {few} must be wider than many {many}");
}

#[test]
fn merge_wait_has_no_later_stage_and_reads_only_its_queue() {
    let mut i = input_at(Stage::MergeWait, 0, 0);
    i.queue = vec![queue(Stage::MergeWait, 2, 4.0, 20)];
    let e = LittleV0.estimate(&i, &StageSamples::default());
    assert_eq!(e.result.unwrap().p50_sec, 1800);
    assert!(e.queue.unwrap().service.is_empty());
}

#[test]
fn a_held_pr_is_refused_blocked_like_every_hold_unaware_heuristic() {
    // #10218: a held PR arrives as `merge_hold`; a queue cannot see a wait
    // for a human, so little-v0 refuses it exactly as it did before the stage.
    let mut i = input_at(Stage::MergeHold, 600, 0);
    i.queue = vec![queue(Stage::MergeHold, 2, 4.0, 20)];
    let e = LittleV0.estimate(&i, &history());
    assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert!(e.result.is_none() && e.queue.is_none() && e.current_stage.is_none());
}

#[test]
fn the_stage_under_an_operator_hold_is_never_read() {
    // #10210: `held` names the stage under an operator hold, for a
    // stall-aware heuristic. little-v0 is not stall-aware: it records the
    // hold's stall unapplied and still refuses `blocked`, whichever way the
    // hold arrives, even with a queue for the stage underneath.
    let operator_hold = StallSignal::new(StallCause::OperatorHold, None);
    let under = input_at(Stage::MergeWait, 600, 0);
    let CurrentState::At(stage_under) = under.current.clone() else {
        unreachable!()
    };
    for current in [
        CurrentState::Refused(NoEstimateReason::Blocked),
        input_at(Stage::MergeHold, 300, 0).current,
    ] {
        let mut i = under.clone();
        i.current = current;
        i.held = Some(stage_under.clone());
        i.stalls = vec![operator_hold.clone()];
        i.queue = vec![queue(Stage::MergeWait, 2, 4.0, 20)];
        let e = LittleV0.estimate(&i, &history());
        assert_eq!(e.no_estimate_reason, Some(NoEstimateReason::Blocked));
        assert!(e.result.is_none() && e.queue.is_none() && e.current_stage.is_none());
        let stalled = e.stalled.as_ref().expect("the hold is recorded");
        assert_eq!(stalled.cause, StallCause::OperatorHold);
        assert!(!stalled.applied);
    }
}

fn roster(repo: &str, pr: u32, stage: Stage, entered: DateTime<Utc>) -> RosterEntry {
    RosterEntry {
        repo: repo.to_string(),
        pr,
        stage: Some(stage),
        entered_at: entered,
        known_at: as_of() - Duration::minutes(1),
    }
}

fn exit(repo: &str, stage: Stage, ago_min: i64) -> StageEvent {
    StageEvent {
        repo: repo.to_string(),
        pr: None,
        stage: Some(stage),
        kind: EventKind::Exit,
        at: as_of() - Duration::minutes(ago_min),
        known_at: as_of() - Duration::minutes(1),
    }
}

#[test]
fn stage_queue_scopes_by_stage_and_reads_only_the_past() {
    let t = as_of();
    let scope = vec!["a/x".to_string(), "b/y".to_string()];
    let ros = vec![
        roster("a/x", 1, Stage::ReviewWait, t - Duration::hours(3)),
        roster("b/y", 2, Stage::ReviewWait, t - Duration::hours(2)),
        roster("b/y", 3, Stage::ReviewWait, t - Duration::hours(1)),
        roster("a/x", 4, Stage::MergeWait, t - Duration::hours(5)),
    ];
    let log = EventLog {
        from: None,
        events: vec![
            exit("a/x", Stage::ReviewWait, 30),
            exit("b/y", Stage::ReviewWait, 90),
            exit("b/y", Stage::MergeWait, 60),
            // Future of as_of: ignored.
            StageEvent {
                at: t + Duration::minutes(5),
                known_at: t + Duration::minutes(5),
                ..exit("a/x", Stage::ReviewWait, 0)
            },
            // Older than the window: ignored.
            exit("a/x", Stage::ReviewWait, 25 * 60),
        ],
    };
    let review =
        stage_queue("a/x", 9, Stage::ReviewWait, t - Duration::minutes(30), &ros, &log, &scope, t)
            .unwrap();
    assert_eq!((review.scope, review.items_ahead, review.exits), (QueueScope::Fleet, 3, 2));
    let merge =
        stage_queue("a/x", 9, Stage::MergeWait, t - Duration::minutes(30), &ros, &log, &scope, t)
            .unwrap();
    assert_eq!((merge.scope, merge.items_ahead, merge.exits), (QueueScope::Repo, 1, 0));
    assert_eq!(merge.drain_rate_per_hr, 0.0);
    assert!(stage_queue("a/x", 9, Stage::SweepBuilder, t, &ros, &log, &scope, t).is_none());
    assert!(stage_queue("c/z", 9, Stage::ReviewWait, t, &ros, &log, &scope, t).is_none());
}

#[test]
fn weighted_rate_reads_a_steady_rate_as_itself() {
    // One exit every 30 minutes across the whole window: 2 per hour.
    let ages: Vec<i64> = (0..48).map(|i| i * 1800 + 900).collect();
    let rate = weighted_rate_per_hr(&ages, HALF_LIFE_SEC, WINDOW_SEC);
    assert!((rate - 2.0).abs() < 0.05, "{rate}");
    assert_eq!(weighted_rate_per_hr(&[], HALF_LIFE_SEC, WINDOW_SEC), 0.0);
    // Recent exits weigh more than old ones.
    assert!(
        weighted_rate_per_hr(&[60], HALF_LIFE_SEC, WINDOW_SEC)
            > weighted_rate_per_hr(&[20 * 3600], HALF_LIFE_SEC, WINDOW_SEC)
    );
}

#[test]
fn registered_beside_land_v2_never_current_and_listed_by_backtest_compare() {
    let registry = Registry::builtin();
    assert!(registry.ids().contains(&LITTLE_V0));
    assert!(registry.for_kind(Kind::Land).any(|h| h.id() == LITTLE_V0));
    assert_ne!(Registry::default_current(Kind::Land), LITTLE_V0);
    assert_ne!(registry.current(Kind::Land, None).id(), LITTLE_V0);

    let t = as_of();
    let case = ReplayCase {
        subject: subject(),
        as_of: t,
        stage: Stage::ReviewWait,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at: t + Duration::seconds(5000),
        dispatch: None,
        age_sec: 0,
        queue: vec![queue(Stage::ReviewWait, 4, 2.0, 12)],
    };
    let mut h = history();
    for stage in [
        Stage::ReviewWait,
        Stage::SweepBuilder,
        Stage::SweepCurator,
        Stage::Doctor,
    ] {
        for i in 0..10_i64 {
            h.stages.push(StageSample {
                repo: "rjwalters/loom".to_string(),
                stage,
                duration_sec: 300 + i * 30,
                observed_at: t - Duration::hours(i + 1),
                source: SampleSource::SweepOutcome,
                host: "host-a".to_string(),
                worked: None,
            });
        }
    }
    let cmp = backtest::compare(&LandV2, &LittleV0, &h, &[case], Filter::default(), &provenance())
        .expect("same kind");
    assert_eq!(cmp.a.heuristic, "land-v2");
    assert_eq!(cmp.b.heuristic, LITTLE_V0);
    assert_eq!(cmp.b.overall.scored, 1, "the queue reaches the estimate through the case");
}

#[test]
fn an_explicit_little_v0_configuration_never_selects_it_as_current() {
    let registry = Registry::builtin();
    assert_eq!(
        registry.current(Kind::Land, Some(LITTLE_V0)).id(),
        Registry::default_current(Kind::Land)
    );
}

#[test]
fn little_v0_is_never_promoted_even_when_both_gates_pass() {
    use super::shadow::{comparison, ledger_with};
    use crate::eta::heuristics::{LAND_V1, LAND_V2};
    use crate::eta::shadow::{self, GateStatus, MIN_LIVE_PAIRS};

    let stats = ledger_with(MIN_LIVE_PAIRS, 100.0, 60.0, MIN_LIVE_PAIRS / 2).stats(
        Kind::Land,
        LAND_V1,
        LAND_V2,
    );
    let passing = comparison(1000.0, 800.0, 40);

    // Control: the same evidence promotes an ordinary candidate.
    let control = shadow::evaluate(Kind::Land, LAND_V1, LAND_V2, Some(&passing), &stats, as_of());
    assert!(control.promote, "{}", control.reason);

    // Relabel the candidate side as little-v0: both numerical gates still pass.
    let mut floor = passing;
    floor.b.heuristic = LITTLE_V0.to_string();
    floor.better = Some(LITTLE_V0.to_string());
    let decision = shadow::evaluate(Kind::Land, LAND_V1, LITTLE_V0, Some(&floor), &stats, as_of());
    assert_eq!(decision.backtest.status, GateStatus::Passed, "{}", decision.backtest.detail);
    assert_eq!(decision.live.status, GateStatus::Passed);
    assert!(!decision.promote);
    assert!(decision.reason.contains("baseline"), "{}", decision.reason);
}
