//! Stalls and the residual-life tail (#10210): `land-v4`, the `stalled`
//! explanation field, and the `beyond_history` replacement.

use super::{as_of, input_at, provenance, subject};
use crate::eta::backtest::{self, Filter, ReplayCase, TAIL_EXTRAPOLATED, TAIL_NONE};
use crate::eta::explanation::{CONDITIONING_RESIDUAL_LIFE, CONDITIONING_TRUNCATE};
use crate::eta::heuristics::{LandV1, LandV2, LandV4, LAND_V1, LAND_V4};
use crate::eta::history::{SampleSource, StageSample, StageSamples, VerdictSample};
use crate::eta::labels::{held_only_by_operator, operator_hold_label, stage_ignoring_holds};
use crate::eta::score::{EstimateSummary, OutcomeKind};
use crate::eta::simulate::{run_explanation, STALLED_SHARE_KEY};
use crate::eta::stall::{
    binding, default_term_sec, host_signals, StallCause, StallSignal, StallSnapshot,
    TERM_BASIS_DEFAULT, TERM_BASIS_RESUME_AT,
};
use crate::eta::tracker::{EstimateContext, PrView, Tracker};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason,
    Registry, Stage, MIN_SAMPLES,
};
use crate::rate_limit_breaker::{BreakerPhase, RateLimitSnapshot};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

/// `n` samples per post-dispatch stage (`base, 2·base, …`) and `n`
/// first-attempt verdicts, a third of them rejections.
fn synthetic(n: usize, base: i64) -> StageSamples {
    let mut samples = StageSamples::default();
    for stage in Stage::ALL {
        for i in 0..n {
            samples.stages.push(StageSample {
                repo: REPO.to_string(),
                stage,
                duration_sec: base * (i as i64 + 1),
                observed_at: as_of() - Duration::hours(i as i64 + 1),
                source: SampleSource::SweepOutcome,
                host: "host-a".to_string(),
                worked: None,
            });
        }
    }
    for i in 0..n {
        samples.verdicts.push(VerdictSample {
            repo: REPO.to_string(),
            attempt: 1,
            rejected: i % 3 == 0,
            observed_at: as_of() - Duration::hours(i as i64 + 1),
        });
    }
    samples
}

fn at(offset_sec: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(offset_sec)
}

fn with_stall(mut input: EstimateInput, signal: StallSignal) -> EstimateInput {
    input.stalls = vec![signal];
    input
}

fn breaker(suppressed: bool, until: Option<DateTime<Utc>>, core: Option<u64>) -> RateLimitSnapshot {
    RateLimitSnapshot {
        enabled: true,
        phase: if suppressed {
            BreakerPhase::Cooldown
        } else {
            BreakerPhase::Closed
        },
        suppressed,
        source: Some("work_finder".to_string()),
        tripped_at: suppressed.then(|| at(-60)),
        cooldown_until: until,
        trips_total: 1,
        core_remaining: core,
        graphql_remaining: Some(4000),
        core_used: None,
        graphql_used: None,
        budget_probed_at: Some(at(-60)),
    }
}

// ------------------------------------------------------ the stall record

#[test]
fn binding_stall_per_cause_known_and_unknown_resume() {
    for cause in StallCause::ALL {
        let known = binding(&[StallSignal::new(cause, Some(at(1800)))], as_of()).unwrap();
        assert_eq!(known.cause, cause);
        assert_eq!(known.resume_at, Some(at(1800)));
        assert_eq!(known.term_sec, 1800, "{cause}: resume_at - as_of");
        assert_eq!(known.term_basis, TERM_BASIS_RESUME_AT);
        assert!(!known.applied, "binding() only records; a heuristic applies");

        let unknown = binding(&[StallSignal::new(cause, None)], as_of()).unwrap();
        assert_eq!(unknown.resume_at, None);
        assert_eq!(unknown.term_sec, default_term_sec(cause), "{cause}: documented default");
        assert!(unknown.term_sec > 0, "{cause}: a default is never zero");
        assert_eq!(unknown.term_basis, TERM_BASIS_DEFAULT);
    }
    assert_eq!(binding(&[], as_of()), None, "no signal, no stall");
    // A resume instant already past is a stall about to clear: zero term.
    let past = binding(&[StallSignal::new(StallCause::BreakerCooldown, Some(at(-5)))], as_of());
    assert_eq!(past.unwrap().term_sec, 0);
}

#[test]
fn the_longest_stall_binds_and_the_rest_are_named() {
    let signals = [
        StallSignal::new(StallCause::BreakerCooldown, Some(at(600))),
        StallSignal::new(StallCause::OperatorHold, None).with_detail("loom:operator"),
        StallSignal::new(StallCause::RateLimitQuota, Some(at(1200))),
    ];
    let stalled = binding(&signals, as_of()).unwrap();
    assert_eq!(stalled.cause, StallCause::OperatorHold, "a day beats 20 minutes");
    assert_eq!(stalled.detail.as_deref(), Some("loom:operator"));
    assert_eq!(stalled.also, vec![StallCause::BreakerCooldown, StallCause::RateLimitQuota]);
}

#[test]
fn every_cause_round_trips_its_wire_name() {
    for cause in StallCause::ALL {
        let json = serde_json::to_value(cause).unwrap();
        assert_eq!(json, serde_json::Value::String(cause.as_str().to_string()));
        assert_eq!(serde_json::from_value::<StallCause>(json).unwrap(), cause);
    }
}

// ------------------------------------------------------ land-v4: the term

#[test]
fn land_v4_carries_stalled_for_every_cause_known_and_unknown() {
    let history = synthetic(40, 60);
    for cause in StallCause::ALL {
        for resume_at in [Some(at(3600)), None] {
            let input =
                with_stall(input_at(Stage::ReviewWait, 0, 0), StallSignal::new(cause, resume_at));
            let explanation = LandV4.estimate(&input, &history);
            assert_eq!(explanation.no_estimate_reason, None, "{cause}");
            let stalled = explanation.stalled.as_ref().expect("stalled recorded");
            assert_eq!(stalled.cause, cause);
            assert_eq!(stalled.resume_at, resume_at);
            assert!(stalled.applied, "{cause}: land-v4 applies the term");
            let expected = resume_at.map_or(default_term_sec(cause), |_| 3600);
            assert_eq!(stalled.term_sec, expected);
            // The explanation alone recomputes the stalled numbers.
            assert_eq!(run_explanation(&explanation), explanation.quantiles_with_p90(), "{cause}");
            // And it survives the wire.
            let parsed: crate::eta::Explanation =
                serde_json::from_str(&serde_json::to_string(&explanation).unwrap()).unwrap();
            assert_eq!(parsed, explanation);
        }
    }
}

#[test]
fn a_known_resume_moves_every_quantile_by_exactly_the_term() {
    let history = synthetic(40, 60);
    let base = LandV4.estimate(&input_at(Stage::ReviewWait, 300, 0), &history);
    let (b25, b50, b75) = base.quantiles().unwrap();
    assert_eq!(base.stalled, None, "no signal, no field");
    let resume = at(7200);
    let stalled = LandV4.estimate(
        &with_stall(
            input_at(Stage::ReviewWait, 300, 0),
            StallSignal::new(StallCause::RateLimitQuota, Some(resume)),
        ),
        &history,
    );
    let (s25, s50, s75) = stalled.quantiles().unwrap();
    let term = (resume - as_of()).num_seconds();
    // The lower bound: p25 >= (T - as_of) + the normal term's p25 — with an
    // unchanged draw stream it is exactly that.
    assert!(s25 >= term + b25);
    assert_eq!((s25, s50, s75), (b25 + term, b50 + term, b75 + term));
    // The stall shows as its own share of the median band.
    let share = stalled.contributions.as_ref().unwrap().p50_share[STALLED_SHARE_KEY];
    assert!(share > 0.5 && share < 1.0, "{share}");
    // The current stage's mark is when service resumes.
    let marks = &stalled.result.as_ref().unwrap().stage_marks;
    let review = marks.iter().find(|m| m.stage == Stage::ReviewWait).unwrap();
    assert_eq!(review.p50_at, Some(resume));
}

#[test]
fn earlier_heuristics_record_the_stall_but_never_apply_it() {
    let history = synthetic(40, 60);
    let plain = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    let input = with_stall(
        input_at(Stage::ReviewWait, 0, 0),
        StallSignal::new(StallCause::TokenPoolExhausted, None),
    );
    let recorded = LandV1.estimate(&input, &history);
    assert_eq!(recorded.quantiles(), plain.quantiles(), "land-v1's numbers never move");
    let stalled = recorded.stalled.as_ref().unwrap();
    assert_eq!(stalled.cause, StallCause::TokenPoolExhausted);
    assert!(!stalled.applied);
    assert_eq!(run_explanation(&recorded), recorded.quantiles_with_p90());
}

#[test]
fn a_refusal_records_the_stall_unapplied() {
    let mut input = with_stall(
        input_at(Stage::ReviewWait, 0, 0),
        StallSignal::new(StallCause::BreakerCooldown, Some(at(900))),
    );
    input.current = CurrentState::Refused(NoEstimateReason::UnknownStage);
    let explanation = LandV4.estimate(&input, &synthetic(40, 60));
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::UnknownStage));
    let stalled = explanation.stalled.as_ref().unwrap();
    assert!(!stalled.applied, "no result for the term to be part of");
}

// ------------------------------------------- land-v4: operator-hold fallback

fn held_input(stall: bool) -> EstimateInput {
    let mut input = input_at(Stage::MergeWait, 0, 0);
    input.current = CurrentState::Refused(NoEstimateReason::Blocked);
    input.held = Some(CurrentStage {
        stage: Stage::MergeWait,
        entered_at: Some(at(-600)),
        age_sec: 600,
        age_source: AgeSource::TrackerObserved,
        rework_rounds: 0,
        episode_entered_at: None,
    });
    if stall {
        input.stalls =
            vec![StallSignal::new(StallCause::OperatorHold, None).with_detail("loom:operator")];
    }
    input
}

#[test]
fn an_operator_held_pr_gets_an_estimate_from_land_v4_only() {
    let history = synthetic(40, 60);
    let v4 = LandV4.estimate(&held_input(true), &history);
    assert_eq!(v4.no_estimate_reason, None, "land-v4 stops refusing operator holds");
    let stalled = v4.stalled.as_ref().unwrap();
    assert_eq!(stalled.cause, StallCause::OperatorHold);
    assert_eq!(stalled.term_sec, default_term_sec(StallCause::OperatorHold));
    assert!(stalled.applied);
    let (p25, _, _) = v4.quantiles().unwrap();
    assert!(p25 >= stalled.term_sec, "the hold's term is a floor");
    assert_eq!(v4.current_stage.as_ref().unwrap().stage, Stage::MergeWait);

    // Every earlier heuristic still refuses, recording the hold.
    let v1 = LandV1.estimate(&held_input(true), &history);
    assert_eq!(v1.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert_eq!(v1.stalled.as_ref().unwrap().cause, StallCause::OperatorHold);

    // Without an operator-hold signal the held stage is not read.
    let unsignalled = LandV4.estimate(&held_input(false), &history);
    assert_eq!(unsignalled.no_estimate_reason, Some(NoEstimateReason::Blocked));

    // An approved held PR reaches every heuristic as `At(MergeHold)` (#10218):
    // land-v4 (which does not model the hold) still reads the stage under it.
    let mut merge_hold = held_input(true);
    merge_hold.current = CurrentState::At(CurrentStage {
        stage: Stage::MergeHold,
        entered_at: Some(at(-300)),
        age_sec: 300,
        age_source: AgeSource::TrackerObserved,
        rework_rounds: 0,
        episode_entered_at: None,
    });
    let v4 = LandV4.estimate(&merge_hold, &history);
    assert_eq!(v4.no_estimate_reason, None, "merge_hold: estimated, not refused");
    assert_eq!(v4.current_stage.as_ref().unwrap().stage, Stage::MergeWait);
    assert!(v4.stalled.as_ref().unwrap().applied);
    let v1 = LandV1.estimate(&merge_hold, &history);
    assert_eq!(v1.no_estimate_reason, Some(NoEstimateReason::Blocked));
}

#[test]
fn operator_hold_labels() {
    let labels = |ls: &[&str]| ls.iter().map(|l| (*l).to_string()).collect::<Vec<_>>();
    assert_eq!(
        operator_hold_label(&labels(&["loom:pr", "loom:operator"])),
        Some("loom:operator")
    );
    assert_eq!(
        operator_hold_label(&labels(&["loom:pr", "loom:operator-priority"])),
        None,
        "the star is not a hold"
    );
    assert!(held_only_by_operator(&labels(&["loom:pr", "loom:operator-only"])));
    assert!(
        !held_only_by_operator(&labels(&["loom:pr", "loom:operator", "loom:blocked"])),
        "a non-operator hold keeps the plain refusal"
    );
    assert!(!held_only_by_operator(&labels(&["loom:pr"])));
    assert_eq!(
        stage_ignoring_holds(&labels(&["loom:pr", "loom:operator"])),
        Ok(Stage::MergeWait)
    );
}

// ------------------------------------------- land-v4: no beyond_history

#[test]
fn beyond_history_is_a_flagged_tail_estimate_for_land_v4() {
    let history = synthetic(40, 60);
    // 2400 s is the longest review_wait; at 2350 s only 1 sample is longer.
    let input = input_at(Stage::ReviewWait, 2350, 0);
    let v2 = LandV2.estimate(&input, &history);
    assert_eq!(v2.no_estimate_reason, Some(NoEstimateReason::BeyondHistory), "v2 unchanged");

    let v4 = LandV4.estimate(&input, &history);
    assert_eq!(v4.no_estimate_reason, None, "land-v4 never refuses beyond_history");
    let result = v4.result.as_ref().unwrap();
    assert!(result.tail_extrapolated, "the tail estimate is flagged");
    let conditioning = v4.stages[0].conditioning.as_ref().unwrap();
    assert_eq!(conditioning.method, CONDITIONING_RESIDUAL_LIFE);
    assert_eq!(conditioning.n_above, 1);
    assert!(result.p50_sec >= 2350, "median remaining >= the age: {}", result.p50_sec);
    assert_eq!(run_explanation(&v4), v4.quantiles_with_p90(), "recomputable from the record");

    // Past every sample (f_age = 1) as well.
    let past = LandV4.estimate(&input_at(Stage::ReviewWait, 5000, 0), &history);
    assert!(past.result.as_ref().unwrap().tail_extrapolated);
    assert!(past.result.as_ref().unwrap().p50_sec >= 5000);

    // Inside the history nothing changes: ordinary truncation, no flag.
    let inside = LandV4.estimate(&input_at(Stage::ReviewWait, 1200, 0), &history);
    assert!(!inside.result.as_ref().unwrap().tail_extrapolated);
    assert_eq!(inside.stages[0].conditioning.as_ref().unwrap().method, CONDITIONING_TRUNCATE);

    // The flag reaches the scoring summary.
    assert!(EstimateSummary::of(&v4).tail_extrapolated);
    assert!(!EstimateSummary::of(&inside).tail_extrapolated);
}

#[test]
fn genuinely_unknown_stages_still_refuse_for_land_v4() {
    let mut unknown = input_at(Stage::ReviewWait, 0, 0);
    unknown.current = CurrentState::Refused(NoEstimateReason::UnknownStage);
    assert_eq!(
        LandV4
            .estimate(&unknown, &synthetic(40, 60))
            .no_estimate_reason,
        Some(NoEstimateReason::UnknownStage)
    );
    let thin = synthetic(MIN_SAMPLES - 1, 60);
    assert_eq!(
        LandV4
            .estimate(&input_at(Stage::ReviewWait, 0, 0), &thin)
            .no_estimate_reason,
        Some(NoEstimateReason::InsufficientSamples)
    );
    let mut ready = input_at(Stage::ReadyWait, 0, 0);
    ready.dispatch = None;
    assert_eq!(
        LandV4
            .estimate(&ready, &synthetic(40, 60))
            .no_estimate_reason,
        Some(NoEstimateReason::NoDispatchPlan)
    );
}

#[test]
fn the_backtest_buckets_tail_estimates_apart() {
    let history = synthetic(40, 60);
    let case = |age_sec: i64| ReplayCase {
        subject: subject(),
        as_of: as_of(),
        stage: Stage::ReviewWait,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at: at(4000),
        dispatch: None,
        age_sec,
    };
    let cases = [case(0), case(2350)];
    let v4 = backtest::run(&LandV4, &history, &cases, Filter::default(), &provenance());
    assert_eq!(v4.overall.n, 2);
    assert_eq!(v4.overall.refused, 0, "land-v4 answers both");
    assert_eq!(v4.by_tail[TAIL_EXTRAPOLATED].n, 1);
    assert_eq!(v4.by_tail[TAIL_NONE].n, 1);
    assert!(v4.by_tail[TAIL_EXTRAPOLATED]
        .mean_pinball_loss_sec
        .is_some());

    let v2 = backtest::run(&LandV2, &history, &cases, Filter::default(), &provenance());
    assert_eq!(v2.overall.refused, 1, "land-v2 refuses the aged case");
    assert!(v2.by_tail.is_empty(), "no tail case, no split: the report shape is unchanged");
    let json = serde_json::to_value(&v2).unwrap();
    assert!(json.get("by_tail").is_none());
}

// ------------------------------------------------------- signal sources

#[test]
fn host_signals_from_the_breaker_the_ledger_and_the_pool() {
    let now = as_of();
    // A cooldown with a pool at zero is the quota itself.
    let quota = host_signals(Some(&breaker(true, Some(at(1500)), Some(0))), &[], false, now);
    assert_eq!(
        quota,
        vec![
            StallSignal::new(StallCause::RateLimitQuota, Some(at(1500))).with_detail("work_finder")
        ]
    );
    // A cooldown with budget left (a secondary limit) is the breaker.
    let cool = host_signals(Some(&breaker(true, Some(at(300)), Some(12))), &[], false, now);
    assert_eq!(cool[0].cause, StallCause::BreakerCooldown);
    assert_eq!(cool[0].resume_at, Some(at(300)));
    // A closed breaker is no stall; a ledger pool at zero is, at its reset.
    let ledger = host_signals(
        Some(&breaker(false, None, Some(0))),
        &[("core", Some(at(900))), ("graphql", Some(at(2000)))],
        false,
        now,
    );
    assert_eq!(
        ledger,
        vec![StallSignal::new(StallCause::RateLimitQuota, Some(at(2000))).with_detail("graphql")]
    );
    // The breaker's quota reading is not doubled by the ledger's.
    let both = host_signals(
        Some(&breaker(true, Some(at(1500)), Some(0))),
        &[("core", Some(at(900)))],
        false,
        now,
    );
    assert_eq!(both.len(), 1);
    // The empty-pool brake has no resume instant.
    let pool = host_signals(None, &[], true, now);
    assert_eq!(pool, vec![StallSignal::new(StallCause::TokenPoolExhausted, None)]);
    assert!(host_signals(None, &[], false, now).is_empty());
}

#[test]
fn for_item_narrows_the_snapshot() {
    let mut snapshot = StallSnapshot::default();
    snapshot
        .host
        .push(StallSignal::new(StallCause::BreakerCooldown, Some(at(60))));
    snapshot.locked_repos.insert(REPO.to_string());
    let labels = vec!["loom:issue".to_string()];
    let ready = snapshot.for_item("RJWalters/Loom", Some(Stage::ReadyWait), &labels);
    let causes: Vec<StallCause> = ready.iter().map(|s| s.cause).collect();
    assert_eq!(causes, vec![StallCause::BreakerCooldown, StallCause::PrOpenLockout]);
    // The lockout freezes dispatch only: a PR in review is not behind it.
    let review = snapshot.for_item(REPO, Some(Stage::ReviewWait), &labels);
    assert_eq!(review.len(), 1);
    let held = snapshot.for_item(
        REPO,
        Some(Stage::MergeWait),
        &["loom:pr".to_string(), "loom:operator".to_string()],
    );
    assert_eq!(held[1].cause, StallCause::OperatorHold);
    assert_eq!(held[1].detail.as_deref(), Some("loom:operator"));
}

// ------------------------------------------------------- the tracker path

#[test]
fn the_tracker_feeds_stalls_and_the_held_stage() {
    let registry = Registry::builtin();
    let history = synthetic(40, 60);
    let repo_ids = BTreeMap::new();
    let mut stalls = StallSnapshot::default();
    stalls
        .host
        .push(StallSignal::new(StallCause::RateLimitQuota, Some(at(1800))));
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &stalls,
    };
    let mut tracker = Tracker::new(provenance());
    let pr = |labels: &[&str]| PrView {
        number: 9301,
        issue: 9289,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some(at(-7200)),
        updated_at: Some(at(-600)),
    };
    tracker.on_listing(REPO, &[pr(&["loom:pr", "loom:operator"])], as_of(), 300);
    let emissions = tracker.estimate(None, &ctx, as_of());
    let by_id = |id: &str| {
        emissions
            .iter()
            .find(|e| e.explanation.heuristic == id)
            .map(|e| &e.explanation)
            .unwrap()
    };
    let v1 = by_id(LAND_V1);
    assert_eq!(v1.no_estimate_reason, Some(NoEstimateReason::Blocked), "primary unchanged");
    let v4 = by_id(LAND_V4);
    assert_eq!(v4.no_estimate_reason, None, "operator-held PR estimated");
    let stalled = v4.stalled.as_ref().unwrap();
    assert_eq!(stalled.cause, StallCause::OperatorHold, "a day outlasts the quota");
    assert_eq!(stalled.also, vec![StallCause::RateLimitQuota]);
    assert_eq!(v4.current_stage.as_ref().unwrap().stage, Stage::MergeWait);

    // The hold lifts: the quota is the only stall left.
    tracker.on_listing(REPO, &[pr(&["loom:pr"])], at(60), 300);
    let later = tracker.estimate(None, &ctx, at(400));
    let v4 = later
        .iter()
        .find(|e| e.explanation.heuristic == LAND_V4)
        .map(|e| &e.explanation)
        .unwrap();
    let stalled = v4.stalled.as_ref().unwrap();
    assert_eq!(stalled.cause, StallCause::RateLimitQuota);
    assert_eq!(stalled.term_sec, 1400);
}
