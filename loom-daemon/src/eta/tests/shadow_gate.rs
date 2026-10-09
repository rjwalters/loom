//! The promotion gate measures what users feel (#10233, Slice B PR 1): the
//! common decidable subset, late surprises (including censored ones), answer
//! rates counted per tracker pass, the per-day win rate, and the ledger
//! defects that would have silently reset or biased all of it.

use super::shadow::{BACKTEST_CASES, PASSING_PAIRS};
use super::{as_of, history_a, input_at, provenance};
use crate::eta::backtest::Comparison;
use crate::eta::heuristics::{LandV1, LAND_V1, LAND_V2, LAND_V4};
use crate::eta::history::{SampleSource, StageSample};
use crate::eta::score::{score, EstimateSummary, OutcomeKind};
use crate::eta::shadow::{
    self, wilson, GateStatus, PromotionDecision, ShadowLedger, DECISION_SCHEMA, MIN_FOLDS,
};
use crate::eta::tracker::{
    censor, EstimateContext, ItemKey, PassAnswers, PrState, PrView, ReadyPlan, ReadyRow, Resolved,
    Tracker, CENSOR_SOURCE, PENDING_MAX_AGE_DAYS, SLOT_TURNOVER_REPO,
};
use crate::eta::{Heuristic, Kind, Registry, Stage};
use crate::types::{DispatchPlanContext, PlanGate, PlanSlots, PlanState, RowPlan};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

// ---------------------------------------------------------------- fixtures

/// A `heuristic` estimate at `as_of() + offset` with p25/p50/p75/p90.
fn summary(heuristic: &str, offset: i64, q: (i64, i64, i64, Option<i64>)) -> EstimateSummary {
    let mut explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    explanation.heuristic = heuristic.to_string();
    explanation.as_of = as_of() + Duration::seconds(offset);
    let mut s = EstimateSummary::of(&explanation);
    s.p25_sec = Some(q.0);
    s.p50_sec = Some(q.1);
    s.p75_sec = Some(q.2);
    s.p90_sec = q.3;
    s
}

fn resolved(estimate: EstimateSummary, outcome: OutcomeKind, at: DateTime<Utc>) -> Resolved {
    Resolved {
        score: score(&estimate, outcome, at, &[]),
        estimate,
        outcome_source: "pulls_read".to_string(),
        outcome_resolution_sec: Some(0),
        result: None,
    }
}

fn current_land(_: Kind) -> String {
    LAND_V1.to_string()
}

/// One paired observation, scores set directly.
#[derive(Clone, Copy)]
struct Side {
    loss: f64,
    covered: bool,
    late: bool,
}

const fn side(loss: f64, covered: bool, late: bool) -> Side {
    Side {
        loss,
        covered,
        late,
    }
}

/// Record a pair of `current` and `candidate` for `issue` at `as_of() + offset`.
fn record(ledger: &mut ShadowLedger, issue: u32, offset: i64, current: Side, candidate: Side) {
    let at = as_of() + Duration::seconds(offset);
    let q = (0, 0, 0, Some(0));
    let mut group = vec![
        resolved(summary(LAND_V1, offset, q), OutcomeKind::Landed, at),
        resolved(summary(LAND_V2, offset, q), OutcomeKind::Landed, at),
    ];
    for (r, s) in group.iter_mut().zip([current, candidate]) {
        r.score.pinball_loss_sec = Some(s.loss);
        r.score.pinball4_loss_sec = Some(s.loss);
        r.score.covered = Some(s.covered);
        r.score.above_p90 = Some(s.late);
        r.estimate.issue = issue;
    }
    ledger.record(&current_land, &group);
}

fn answers(passes: usize, current: bool, candidate: bool) -> Vec<PassAnswers> {
    let pass = PassAnswers {
        kind: Kind::Land,
        states: vec![
            (LAND_V1.to_string(), current),
            (LAND_V2.to_string(), candidate),
        ],
    };
    vec![pass; passes]
}

/// `n` pairs over `days` UTC days, each its own item; pair `i` is `f(i)`.
fn ledger(n: usize, days: i64, f: impl Fn(usize) -> (Side, Side)) -> ShadowLedger {
    let mut ledger = ShadowLedger::default();
    for i in 0..n {
        let offset = (i as i64 % days) * 86_400 + i as i64 * 10;
        let (current, candidate) = f(i);
        record(&mut ledger, 10_000 + i as u32, offset, current, candidate);
    }
    ledger.record_answers(&current_land, &answers(n, true, true));
    ledger
}

/// The candidate is uniformly better on loss, half-covered, never late.
fn winning(i: usize) -> (Side, Side) {
    (side(100.0, true, false), side(60.0, i.is_multiple_of(2), false))
}

/// A backtest the candidate wins, on every walk-forward fold.
fn won_backtest() -> Comparison {
    super::shadow::comparison(1000.0, 800.0, BACKTEST_CASES)
}

fn decide(ledger: &ShadowLedger) -> PromotionDecision {
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    shadow::evaluate(Kind::Land, LAND_V1, LAND_V2, Some(&won_backtest()), &stats, as_of())
}

#[test]
fn the_fixture_ledger_passes_on_its_own() {
    let decision = decide(&ledger(PASSING_PAIRS, 10, winning));
    assert!(decision.promote, "{}", decision.reason);
    assert_eq!(decision.live.stats.day_wins.days, 10);
}

// ---------------------------------------------------- the PairSums defect

/// The `shadow.json` shape persisted before #10233. It has none of the new
/// fields; it must parse with its counts intact rather than fall back to an
/// empty ledger (which silently restarted every host's 50-pair count).
const OLD_SHAPE_LEDGER: &str = r#"{
  "pairs": {"land|land-v1|land-v2": {"pairs": 50, "current_loss_sec": 5000.0,
    "candidate_loss_sec": 3000.0, "current_covered": 24, "candidate_covered": 26}},
  "keys": {"land|land-v1|land-v2": {"kind": "land", "current": "land-v1", "candidate": "land-v2"}}
}"#;

#[test]
fn an_old_shape_ledger_parses_with_its_counts_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = shadow::ledger_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, OLD_SHAPE_LEDGER).unwrap();

    let ledger = shadow::read_ledger(&path).expect("the pre-#10233 shape still parses");
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.pairs, 50, "the 50-pair count survives the upgrade");
    assert_eq!(stats.current_mean_pinball_loss_sec, Some(100.0));
    assert_eq!(stats.candidate_mean_pinball_loss_sec, Some(60.0));
    assert_eq!(stats.candidate_coverage, Some(0.52));
    // The new figures start from zero — absent, not invented.
    assert_eq!((stats.loss4_pairs, stats.late_pairs, stats.answer_pairs), (0, 0, 0));
    assert_eq!(stats.candidate_late_rate, None);

    // The daemon's load agrees, and touches nothing.
    let (loaded, note) = shadow::load_ledger(&path, as_of());
    assert_eq!((loaded, note), (ledger, None));
    assert!(path.exists());
}

#[test]
fn an_unreadable_ledger_is_set_aside_loudly_never_silently_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = shadow::ledger_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let corrupt = OLD_SHAPE_LEDGER.replace("\"pairs\": 50", "\"pairs\": \"fifty\"");
    std::fs::write(&path, &corrupt).unwrap();

    assert!(shadow::read_ledger(&path).is_err(), "a parse failure is an error, not empty");
    let (ledger, note) = shadow::load_ledger(&path, as_of());
    assert_eq!(ledger, ShadowLedger::default());
    let note = note.expect("the failure is reported to the caller to log");
    assert!(note.contains("NOT carried"), "{note}");

    // The original bytes are kept beside it, so the next persist cannot
    // overwrite the only copy of the counts.
    assert!(!path.exists());
    let aside = path.with_extension("json.unreadable-20260920T120000Z");
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), corrupt);
    shadow::write_ledger(&path, &ledger).unwrap();
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), corrupt);
}

#[test]
fn an_old_decision_record_still_parses_and_the_schema_tag_is_unchanged() {
    let decision = decide(&ledger(PASSING_PAIRS, 10, winning));
    assert_eq!(decision.schema, DECISION_SCHEMA);
    assert_eq!(DECISION_SCHEMA, "eta-promotion-decision/v1", "additive, so still v1");
    assert_eq!(decision.live.min_folds, MIN_FOLDS);

    let mut old = serde_json::to_value(&decision).unwrap();
    let live = old["live"].as_object_mut().unwrap();
    for key in ["min_folds", "late_surprise_slack", "answer_rate_slack"] {
        live.remove(key).unwrap();
    }
    let stats = live["stats"].as_object_mut().unwrap();
    for key in [
        "loss4_pairs",
        "current_mean_pinball4_loss_sec",
        "candidate_mean_pinball4_loss_sec",
        "late_pairs",
        "current_late_rate",
        "candidate_late_rate",
        "answer_pairs",
        "current_answer_rate",
        "candidate_answer_rate",
        "day_wins",
    ] {
        stats.remove(key).unwrap();
    }
    let parsed: PromotionDecision = serde_json::from_value(old).expect("a v1 log line parses");
    assert_eq!(parsed.live.min_folds, 0);
    assert_eq!(parsed.live.stats.pairs, decision.live.stats.pairs);
}

// ------------------------------------------------- the gate's new rules

#[test]
fn a_candidate_that_wins_on_pinball_but_is_late_22_percent_of_the_time_is_refused() {
    // current is late 10% of the time, the candidate 22% — while beating it
    // on loss every single day.
    let late = ledger(PASSING_PAIRS, 10, |i| {
        let (mut current, mut candidate) = winning(i);
        current.late = i.is_multiple_of(10);
        candidate.late = i % 50 < 11;
        (current, candidate)
    });
    let stats = late.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.current_late_rate, Some(0.10));
    assert_eq!(stats.candidate_late_rate, Some(0.22));
    assert!(stats.candidate_mean_pinball4_loss_sec < stats.current_mean_pinball4_loss_sec);

    let decision = decide(&late);
    assert_eq!(decision.live.status, GateStatus::Failed);
    assert!(!decision.promote);
    assert!(
        decision.live.detail.contains("late-surprise rate 22.0%"),
        "{}",
        decision.live.detail
    );

    // One point worse is within the slack, not a regression: 3% vs 2%.
    let close = ledger(100, 10, |i| {
        let (mut current, mut candidate) = winning(i);
        current.late = i < 2;
        candidate.late = i < 3;
        (current, candidate)
    });
    let decision = decide(&close);
    assert!(decision.promote, "{}", decision.reason);
}

#[test]
fn an_answer_rate_regression_is_refused() {
    let mut ledger = ledger(PASSING_PAIRS, 10, winning);
    // Twenty more passes where only `current` answered: 100/120 vs 120/120.
    ledger.record_answers(&current_land, &answers(20, true, false));
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.answer_pairs, 120);
    assert_eq!(stats.current_answer_rate, Some(1.0));
    let decision = decide(&ledger);
    assert!(!decision.promote);
    assert!(decision.live.detail.contains("answer rate 83.3%"), "{}", decision.live.detail);
}

#[test]
fn refusing_the_hard_cases_buys_the_candidate_nothing() {
    // The candidate refuses every case current found hard. On the
    // common decidable subset those cases leave BOTH sides' loss sums, so
    // the refusal is no loss advantage...
    let mut ledger = ledger(PASSING_PAIRS, 10, winning);
    let before = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    for i in 0..20 {
        let offset = 20 * 86_400 + i * 10;
        let at = as_of() + Duration::seconds(offset + 99_999);
        let q = (0, 0, 0, Some(0));
        let mut current = resolved(summary(LAND_V1, offset, q), OutcomeKind::Landed, at);
        current.score.pinball_loss_sec = Some(9_999.0);
        current.score.pinball4_loss_sec = Some(9_999.0);
        let mut refused = summary(LAND_V2, offset, q);
        refused.p25_sec = None;
        refused.p50_sec = None;
        refused.p75_sec = None;
        refused.p90_sec = None;
        let refused = resolved(refused, OutcomeKind::Landed, at);
        ledger.record(&current_land, &[current, refused]);
    }
    let after = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(after.pairs, before.pairs);
    assert_eq!(after.loss4_pairs, before.loss4_pairs);
    assert_eq!(after.current_mean_pinball4_loss_sec, before.current_mean_pinball4_loss_sec);

    // ...and the tracker's per-pass tally counts the refusals, so the gate
    // sees an answer-rate regression instead of a free win.
    ledger.record_answers(&current_land, &answers(20, true, false));
    let decision = decide(&ledger);
    assert!(!decision.promote);
    assert!(decision.live.detail.contains("answer rate"), "{}", decision.live.detail);
}

#[test]
fn one_lucky_day_does_not_carry_the_per_day_win_rate() {
    // Day 0: the candidate is flawless and current terrible. Days 1-9: the
    // candidate is slightly worse. The pooled mean favours the candidate —
    // 54s vs 145s — but it won one day of ten.
    let ledger = ledger(PASSING_PAIRS, 10, |i| {
        if i.is_multiple_of(10) {
            (side(1000.0, true, false), side(0.0, i.is_multiple_of(2), false))
        } else {
            (side(50.0, true, false), side(60.0, i.is_multiple_of(2), false))
        }
    });
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.current_mean_pinball4_loss_sec, Some(145.0));
    assert_eq!(stats.candidate_mean_pinball4_loss_sec, Some(54.0));
    assert_eq!((stats.day_wins.wins, stats.day_wins.days), (1, 10));
    let decision = decide(&ledger);
    assert!(!decision.promote);
    assert!(
        decision.live.detail.contains("per-day win rate 1/10"),
        "{}",
        decision.live.detail
    );
}

#[test]
fn fewer_than_min_folds_decided_days_is_not_enough() {
    let short = ledger(PASSING_PAIRS, MIN_FOLDS as i64 - 1, winning);
    let decision = decide(&short);
    assert!(!decision.promote);
    assert!(
        decision
            .live
            .detail
            .contains(&format!("{} decided day(s)", MIN_FOLDS - 1)),
        "{}",
        decision.live.detail
    );
    assert!(decide(&ledger(PASSING_PAIRS, MIN_FOLDS as i64, winning)).promote);
}

#[test]
fn the_wilson_interval_is_deterministic_and_correct() {
    let close = |a: f64, b: f64| (a - b).abs() < 1e-3;
    let (low, high) = wilson(10, 10);
    assert!(close(low, 0.7225) && close(high, 1.0), "{low} {high}");
    let (low, _) = wilson(7, 7);
    assert!(close(low, 0.6457), "{low}");
    let (low, high) = wilson(5, 7);
    assert!(low < 0.5 && high > 0.9, "{low} {high}: five of seven is not significant");
    assert_eq!(wilson(5, 7), wilson(5, 7));
}

#[test]
fn refreshing_the_same_items_manufactures_no_live_evidence() {
    // 600 winning pairs over 10 days, but only 30 items refreshed 20 times
    // each: plenty of pairs, nowhere near enough independent items.
    let mut refreshed = ShadowLedger::default();
    for i in 0..600_usize {
        let offset = (i as i64 % 10) * 86_400 + i as i64 * 10;
        let (current, candidate) = winning(i);
        record(&mut refreshed, 10_000 + (i % 30) as u32, offset, current, candidate);
    }
    refreshed.record_answers(&current_land, &answers(600, true, true));
    let stats = refreshed.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.loss4_pairs, 600);
    let item = stats.item_test.as_ref().unwrap();
    assert_eq!((item.distinct_items, item.observations), (30, 600));
    let decision = decide(&refreshed);
    assert!(!decision.promote);
    assert!(decision.live.detail.contains("30 distinct item(s)"), "{}", decision.live.detail);
}

#[test]
fn the_per_item_sums_are_bounded_and_evict_the_least_recently_seen() {
    let mut ledger = ShadowLedger::default();
    let n = shadow::MAX_ITEMS + 5;
    for i in 0..n {
        let offset = i as i64 * 600;
        record(&mut ledger, i as u32, offset, side(100.0, true, false), side(60.0, true, false));
    }
    let items = ledger.items.values().next().unwrap();
    assert_eq!(items.len(), shadow::MAX_ITEMS);
    assert!(!items.keys().any(|k| k.ends_with("#0")), "the oldest item went first");
}

#[test]
fn the_per_day_folds_are_bounded() {
    let ledger = ledger(400, 200, winning);
    assert_eq!(ledger.days.values().next().unwrap().len(), shadow::MAX_DAYS);
}

// ------------------------------------------------ expiry censoring defect

#[test]
fn an_expiring_estimate_past_its_p90_is_scored_as_a_late_surprise_first() {
    let mut tracker = Tracker::new(provenance());
    let hour = 3600;
    tracker.restore_pending(
        vec![
            // p90 one hour: decided late long before it expires.
            summary(LAND_V1, 0, (600, 1200, 1800, Some(hour))),
            // p90 of 40 days: not yet behind it at expiry — undecided.
            summary(LAND_V2, 0, (600, 1200, 1800, Some(40 * 24 * hour))),
            // No p90 recorded: undecided.
            summary(LAND_V4, 0, (600, 1200, 1800, None)),
        ],
        &Registry::builtin(),
    );
    let early = tracker.expire(as_of() + Duration::days(1));
    assert_eq!((early.dropped, early.censored.len()), (0, 0), "nothing expires early");

    let now = as_of() + Duration::days(PENDING_MAX_AGE_DAYS + 1);
    let expired = tracker.expire(now);
    assert_eq!(expired.dropped, 3);
    assert!(tracker.pending().is_empty());
    assert_eq!(expired.censored.len(), 1, "only the decided late surprise is scored");
    let late = &expired.censored[0];
    assert_eq!(late.estimate.heuristic, LAND_V1);
    assert_eq!(late.score.outcome, OutcomeKind::Censored);
    assert_eq!(late.score.above_p90, Some(true));
    assert_eq!(late.score.actual_at, now);
    assert_eq!(late.outcome_source, CENSOR_SOURCE);
    // Only a lower bound on the actual is known: no loss, no error.
    assert_eq!(late.score.pinball_loss_sec, None);
    assert_eq!(late.score.pinball4_loss_sec, None);
    assert_eq!(late.score.error_sec, None);
    assert_eq!(late.score.covered, None);
}

#[test]
fn censoring_needs_a_p90_strictly_behind_now() {
    let s = summary(LAND_V1, 0, (600, 1200, 1800, Some(3600)));
    assert!(
        censor(&s, as_of() + Duration::seconds(3600)).is_none(),
        "actual > p90 is strict"
    );
    assert!(censor(&s, as_of() + Duration::seconds(3601)).is_some());
}

#[test]
fn censored_pairs_count_toward_late_surprise_only_when_both_sides_are_decided() {
    let now = as_of() + Duration::days(PENDING_MAX_AGE_DAYS + 1);
    let both = [
        censor(&summary(LAND_V1, 0, (1, 2, 3, Some(3600))), now).unwrap(),
        censor(&summary(LAND_V2, 0, (1, 2, 3, Some(7200))), now).unwrap(),
    ];
    let mut ledger = ShadowLedger::default();
    ledger.record(&current_land, &both);
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!((stats.late_pairs, stats.pairs, stats.loss4_pairs), (1, 0, 0));
    assert_eq!((stats.current_late_rate, stats.candidate_late_rate), (Some(1.0), Some(1.0)));

    // One side censored, the other undecided (dropped, so never resolved):
    // no pair, rather than a late surprise charged to one side only.
    let mut lonely = ShadowLedger::default();
    lonely.record(&current_land, &both[..1]);
    assert!(lonely.all().is_empty());
}

// --------------------------------------------- per-pass answer counting

fn ready_row(issue: u32, state: PlanState, position: Option<u32>) -> ReadyRow {
    ReadyRow {
        repo: "rjwalters/loom".to_string(),
        issue,
        plan: RowPlan {
            plan_state: state,
            position,
            gate: position.map(|_| PlanGate::Capacity),
            ..RowPlan::default()
        },
        disposition: crate::types::QueueDisposition::DeferredCapacity,
        facts: crate::eta::tracker::IssueRow::default(),
        rank: issue as usize,
        detail: None,
    }
}

fn ready_plan() -> ReadyPlan {
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

#[test]
fn answer_rates_are_counted_once_per_pass_not_once_per_emitted_row() {
    let registry = Registry::builtin();
    // history-a plus slot turnovers, so the ready row is answerable.
    let mut history = history_a();
    history.stages.extend((0..12).map(|i| StageSample {
        repo: SLOT_TURNOVER_REPO.to_string(),
        stage: Stage::ReadyWait,
        duration_sec: 300 * (i + 1),
        observed_at: as_of() - Duration::hours(i + 1),
        source: SampleSource::StageJournal,
        host: "host-fixture-a".to_string(),
        worked: None,
    }));
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
        stalls: &super::NO_STALLS,
    };
    let mut tracker = Tracker::new(provenance());
    // Issue 10 is answerable; issue 12 has no plan position, so it is refused
    // `no_dispatch_plan` — a refusal is emitted once and never refreshed.
    let rows = [
        ready_row(10, PlanState::Next, Some(1)),
        ready_row(12, PlanState::Blocked, None),
    ];
    tracker.on_ready_queue(&rows, &ready_plan(), as_of());

    let mut rows_answered = 0;
    let mut rows_total = 0;
    for offset in [0, 60, 300] {
        for e in tracker.estimate(None, &ctx, as_of() + Duration::seconds(offset)) {
            if e.explanation.kind == Kind::Land && e.primary {
                rows_total += 1;
                rows_answered += usize::from(e.explanation.result.is_some());
            }
        }
    }
    // Rows: answered at 0 and refreshed at 300; refused once. 2 / 3.
    assert_eq!((rows_answered, rows_total), (2, 3), "the inflated, row-counted view");

    let passes: Vec<PassAnswers> = tracker
        .drain_answers()
        .into_iter()
        .filter(|p| p.kind == Kind::Land)
        .collect();
    assert_eq!(passes.len(), 6, "three passes, two live items, one vote each");
    let current_answering = passes
        .iter()
        .filter(|p| p.states.iter().any(|(h, a)| h == LAND_V1 && *a))
        .count();
    assert_eq!(current_answering, 3, "the honest rate is 3 / 6, not 2 / 3");
    assert!(tracker.drain_answers().is_empty(), "drained");

    // A targeted (event-driven) estimate is not a pass and votes nothing.
    let keys = tracker.item_keys();
    tracker.estimate(Some(&keys), &ctx, as_of() + Duration::seconds(900));
    assert!(tracker.drain_answers().is_empty());

    // Folded into the ledger, each pass is one paired answer observation.
    let mut ledger = ShadowLedger::default();
    ledger.record_answers(&current_land, &passes);
    let stats = ledger.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!(stats.answer_pairs, 6);
    assert_eq!(stats.current_answer_rate, Some(0.5));
}

// ------------------------------------- retired heuristics (#10484)

/// `land-v3` and `land-2026-10-04-amber-heron` left the registry on
/// 2026-10-06. A ledger and a pending set persisted before that still name
/// them: both must load, the live pairs must be untouched, the restored
/// pending entries for them are dropped before a landing or a censoring can
/// score them (no `eta.outcome`, no new ledger pair), and a promotion
/// evaluation naming a retired candidate never promotes.
#[test]
fn legacy_entries_for_retired_heuristics_load_cleanly_and_are_ignored() {
    let retired = ["land-v3", "land-2026-10-04-amber-heron"];
    let registry = Registry::builtin();
    for id in retired {
        assert!(registry.get(id).is_none(), "{id} is retired");
    }

    let at = as_of() + Duration::seconds(500);
    let current_land = |_: Kind| LAND_V1.to_string();
    let q = (600, 1200, 1800, Some(3600));
    let mut ledger = ShadowLedger::default();
    for id in retired {
        ledger.record(
            &current_land,
            &[
                resolved(summary(LAND_V1, 0, q), OutcomeKind::Landed, at),
                resolved(summary(id, 0, q), OutcomeKind::Landed, at),
            ],
        );
    }
    ledger.record(
        &current_land,
        &[
            resolved(summary(LAND_V1, 1, q), OutcomeKind::Landed, at),
            resolved(summary(LAND_V2, 1, q), OutcomeKind::Landed, at),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let path = shadow::ledger_path(dir.path());
    shadow::write_ledger(&path, &ledger).unwrap();
    let (loaded, note) = shadow::load_ledger(&path, as_of());
    assert_eq!(note, None, "a legacy ledger is not an unreadable one");
    assert_eq!(loaded, ledger);
    assert_eq!(loaded.stats(Kind::Land, LAND_V1, LAND_V2).pairs, 1, "live pairs intact");

    // Pending estimates naming retired ids, persisted with a decided p90 so
    // both scoring paths would take them, are dropped at restore: the item
    // that lands and the item that is censored at expiry score only the
    // registered heuristics, so no `eta.outcome` and no ledger pair names a
    // retired id.
    let landed_issue = 9289;
    let censored_issue = 9290;
    let for_issue = |id: &str, issue: u32| {
        let mut s = summary(id, 0, (600, 1200, 1800, Some(3600)));
        s.issue = issue;
        s
    };
    let mut persisted = Vec::new();
    for issue in [landed_issue, censored_issue] {
        for id in [LAND_V1, LAND_V2].into_iter().chain(retired) {
            persisted.push(for_issue(id, issue));
        }
    }
    let mut tracker = Tracker::new(provenance());
    let dropped = tracker.restore_pending(persisted, &registry);
    assert_eq!(dropped, 4, "both retired ids, on both items");
    assert_eq!(tracker.pending().len(), 4);
    assert!(tracker
        .pending()
        .iter()
        .all(|p| !retired.contains(&p.heuristic.as_str())));

    let repo = "rjwalters/loom";
    let listed = PrView {
        number: 9301,
        issue: landed_issue,
        labels: vec!["loom:pr".to_string()],
        created_at: Some(as_of()),
        updated_at: Some(as_of()),
    };
    tracker.on_listing(repo, &[listed], at, 300);
    let merged = tracker.on_pr_resolved(
        &ItemKey::new(repo, landed_issue),
        PrState::Merged(at),
        at + Duration::seconds(60),
    );
    let landed: Vec<&str> = merged
        .outcomes
        .iter()
        .map(|r| r.estimate.heuristic.as_str())
        .collect();
    assert_eq!(landed, [LAND_V1, LAND_V2], "the landing scores only registered ids");

    let expired = tracker.expire(as_of() + Duration::days(PENDING_MAX_AGE_DAYS + 1));
    assert_eq!(expired.dropped, 2);
    let censored: Vec<&str> = expired
        .censored
        .iter()
        .map(|r| r.estimate.heuristic.as_str())
        .collect();
    assert_eq!(censored, [LAND_V1, LAND_V2], "censoring scores only registered ids");
    assert!(tracker.pending().is_empty());

    // What the daemon folds into the shadow ledger (`note_outcomes`) and
    // emits as `eta.outcome` (`deliver`) is exactly these outcomes.
    let mut live = ShadowLedger::default();
    live.record(&current_land, &merged.outcomes);
    live.record(&current_land, &expired.censored);
    for id in retired {
        let stats = live.stats(Kind::Land, LAND_V1, id);
        assert_eq!((stats.pairs, stats.late_pairs), (0, 0), "no ledger pair for retired {id}");
    }
    let keys: Vec<(String, String)> = live
        .all()
        .into_iter()
        .map(|st| (st.key.current, st.key.candidate))
        .collect();
    assert_eq!(keys, [(LAND_V1.to_string(), LAND_V2.to_string())], "only the live pair");
    let stats = live.stats(Kind::Land, LAND_V1, LAND_V2);
    assert_eq!((stats.pairs, stats.late_pairs), (1, 2), "landed pair + censored late pair");

    // A promotion evaluation for a retired candidate never promotes.
    for id in retired {
        let stats = loaded.stats(Kind::Land, LAND_V1, id);
        let decision = shadow::evaluate(Kind::Land, LAND_V1, id, None, &stats, as_of());
        assert!(!decision.promote, "a retired candidate is never promoted");
    }
}
